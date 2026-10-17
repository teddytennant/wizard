//! Search, web, task, todo, question, skill and the generic fallback.

use agent_core::{TodoStatus, ToolCall, ToolStatus};
use ratatui::style::Style;
use ratatui::text::Span;
use serde_json::Value;
use tuikit::width::truncate;

use super::rows::{
    block, block_width, click_hint, inline_block, text_rows, BlockSpec, Phase, Rows,
};
use super::text::{
    clean_output, collapse, fmt_path, input_args, str_in, titlecase, took, wrap_text,
};
use super::{classify, denied, display_name, inline, Tool};
use crate::ui::session::{Block, RenderCx};
use tuikit::width::display_width;

// ---- glob / grep / list --------------------------------------------------------------------

/// Lines of a search result that are hits, not notices: wizard says `No matches for pattern`,
/// `(empty directory)` and `... [listing truncated]`.
fn hits(output: &str) -> usize {
    let o = clean_output(output);
    if o.starts_with("No matches") || o.starts_with("(empty") {
        return 0;
    }
    o.lines()
        .filter(|l| {
            !l.trim().is_empty()
                && !l.starts_with('…')
                && !l.starts_with("...")
                && !l.starts_with('[')
        })
        .count()
}

pub fn search(c: &ToolCall, cx: &RenderCx) -> Vec<Block> {
    let tool = classify(c);
    let path = str_in(&c.input, &["path", "dir", "directory"]);
    if tool == Tool::List {
        let text = format!("List {}", fmt_path(path.unwrap_or("."), cx.cwd));
        return vec![inline_block(
            inline(c, cx, "→", text, "Listing directory…", true),
            cx,
        )];
    }
    let (name, pending) = if tool == Tool::Glob {
        ("Glob", "Finding files…")
    } else {
        ("Grep", "Searching content…")
    };
    let pat = str_in(&c.input, &["pattern", "glob", "query"])
        .map(String::from)
        .unwrap_or_else(|| c.title.clone());
    let mut text = format!("{name} \"{pat}\"");
    if let Some(p) = path {
        text.push_str(&format!(" in {}", fmt_path(p, cx.cwd)));
    }
    if c.status == ToolStatus::Completed {
        let n = hits(c.output.as_deref().unwrap_or(""));
        if n > 0 {
            text.push_str(&format!(
                " ({n} {})",
                if n == 1 { "match" } else { "matches" }
            ));
        }
    }
    vec![inline_block(
        inline(c, cx, "✱", text, pending, !pat.is_empty()),
        cx,
    )]
}

// ---- webfetch / websearch ------------------------------------------------------------------

/// Numbered results (`1. title`) in a search tool's answer.
fn numbered(output: &str) -> usize {
    output
        .lines()
        .filter(|l| {
            let d = l.chars().take_while(char::is_ascii_digit).count();
            d > 0 && l[d..].starts_with(". ")
        })
        .count()
}

pub fn web(c: &ToolCall, cx: &RenderCx) -> Vec<Block> {
    if classify(c) == Tool::WebFetch {
        let url = str_in(&c.input, &["url"])
            .map(String::from)
            .unwrap_or_else(|| c.title.clone());
        return vec![inline_block(
            inline(
                c,
                cx,
                "%",
                format!("WebFetch {url}"),
                "Fetching from the web…",
                !url.is_empty(),
            ),
            cx,
        )];
    }
    let q = str_in(&c.input, &["query", "q"])
        .map(String::from)
        .unwrap_or_else(|| c.title.clone());
    let label = match c.name.as_str() {
        "x_search" => "X Search",
        "codesearch" => "Code Search",
        _ => "Web Search",
    };
    let mut text = format!("{label} \"{q}\"");
    if c.status == ToolStatus::Completed {
        let n = numbered(c.output.as_deref().unwrap_or(""));
        if n > 0 {
            text.push_str(&format!(" ({n} results)"));
        }
    }
    vec![inline_block(
        inline(c, cx, "◈", text, "Searching web…", !q.is_empty()),
        cx,
    )]
}

// ---- task ----------------------------------------------------------------------------------

fn toolcalls(n: usize) -> String {
    format!("{n} toolcall{}", if n == 1 { "" } else { "s" })
}

pub fn task(c: &ToolCall, kids: &[&ToolCall], cx: &RenderCx) -> Vec<Block> {
    let desc = str_in(&c.input, &["description"])
        .map(String::from)
        .or_else(|| {
            // Wizard's `spawn_subagent` has no description, only the task text.
            str_in(&c.input, &["task", "prompt"])
                .and_then(|t| t.lines().map(str::trim).find(|l| !l.is_empty()))
                .map(|l| {
                    if display_width(l) > 80 {
                        truncate(l, 80)
                    } else {
                        l.to_string()
                    }
                })
        })
        .unwrap_or_else(|| c.title.trim_start_matches("task:").trim().to_string());
    let agent =
        titlecase(str_in(&c.input, &["subagent_type", "subagent", "agent"]).unwrap_or("General"));
    let background = c.input.get("background").and_then(Value::as_bool) == Some(true);
    let running = c.status == ToolStatus::Running;
    let dur = took(
        &c.id,
        matches!(c.status, ToolStatus::Running | ToolStatus::Pending),
        matches!(c.status, ToolStatus::Completed | ToolStatus::Failed),
    );

    let mut lines = vec![format!(
        "{agent} Task{} — {desc}",
        if background { " (background)" } else { "" }
    )];
    if running && !kids.is_empty() {
        let cur = kids.iter().rev().find(|k| {
            matches!(k.status, ToolStatus::Running | ToolStatus::Completed) && !k.title.is_empty()
        });
        match cur {
            Some(k) => lines.push(format!("↳ {} {}", display_name(k), k.title)),
            None => lines.push(format!("↳ {}", toolcalls(kids.len()))),
        }
    }
    if !running && c.status == ToolStatus::Completed {
        let d = dur
            .filter(|d| d.as_millis() >= 100)
            .map(crate::ui::session::fmt_duration);
        match (kids.len(), d) {
            (0, None) => {}
            (0, Some(d)) => lines.push(format!("↳ {d}")),
            (n, Some(d)) => lines.push(format!("↳ {} · {d}", toolcalls(n))),
            (n, None) => lines.push(format!("↳ {}", toolcalls(n))),
        }
    }
    let mut i = inline(
        c,
        cx,
        if c.status == ToolStatus::Completed {
            "✓"
        } else {
            "│"
        },
        lines.join("\n"),
        "Delegating…",
        !desc.is_empty(),
    );
    i.separate = true;
    i.spinner = running;
    if i.phase == Phase::Failed {
        i.separate = true;
    }
    vec![inline_block(i, cx)]
}

// ---- todo ----------------------------------------------------------------------------------

fn todo_items(c: &ToolCall) -> Vec<(String, TodoStatus)> {
    let status = |s: &str| match s {
        "completed" => TodoStatus::Completed,
        "in_progress" => TodoStatus::InProgress,
        _ => TodoStatus::Pending,
    };
    let list = c
        .input
        .get("todos")
        .or_else(|| c.input.get("items"))
        .and_then(Value::as_array);
    if let Some(a) = list {
        let v: Vec<_> = a
            .iter()
            .filter_map(|v| {
                let text = v
                    .get("content")
                    .or_else(|| v.get("text"))
                    .and_then(Value::as_str)?;
                Some((
                    text.to_string(),
                    status(v.get("status").and_then(Value::as_str).unwrap_or("")),
                ))
            })
            .collect();
        if !v.is_empty() {
            return v;
        }
    }
    // A `read` of the list comes back as glyph lines.
    backend_wizard::wire::parse_todo_lines(c.output.as_deref().unwrap_or(""))
        .into_iter()
        .map(|t| (t.text, t.status))
        .collect()
}

pub fn todo(c: &ToolCall, cx: &RenderCx) -> Vec<Block> {
    let t = cx.theme;
    let items = if c.status == ToolStatus::Completed {
        todo_items(c)
    } else {
        Vec::new()
    };
    if items.is_empty() {
        let mut i = inline(
            c,
            cx,
            "⚙",
            "Updating todos…".into(),
            "Updating todos…",
            false,
        );
        i.failure = Some("Todo update failed");
        return vec![inline_block(i, cx)];
    }
    let w = block_width(cx);
    let mut rows: Rows = Vec::new();
    for (text, st) in items {
        let (mark, color) = match st {
            TodoStatus::Completed => ("[✓]", t.text_muted),
            TodoStatus::InProgress => ("[•]", t.warning),
            TodoStatus::Pending => ("[ ]", t.text_muted),
        };
        for (n, l) in wrap_text(&text, w.saturating_sub(4))
            .into_iter()
            .enumerate()
        {
            let prefix = if n == 0 {
                format!("{mark} ")
            } else {
                "    ".to_string()
            };
            rows.push(vec![
                Span::styled(prefix, Style::new().fg(color)),
                Span::styled(l, Style::new().fg(color)),
            ]);
        }
    }
    vec![block(
        BlockSpec {
            title: Some("# Todos".into()),
            spinner_title: false,
            children: vec![rows],
            error: None,
            click: None,
        },
        cx,
    )]
}

// ---- question ------------------------------------------------------------------------------

fn questions(c: &ToolCall) -> Vec<String> {
    c.input
        .get("questions")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|q| q.get("question").and_then(Value::as_str).map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// opencode answers a question call with `"<question>"="<answer>"` pairs; take the answer
/// that follows each asked question.
fn answer_for(output: &str, q: &str) -> Option<String> {
    let key = format!("\"{q}\"=\"");
    let rest = &output[output.find(&key)? + key.len()..];
    let end = rest
        .find("\", ")
        .or_else(|| rest.find("\". "))
        .or_else(|| rest.rfind('"'))
        .unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

pub fn question(c: &ToolCall, cx: &RenderCx) -> Vec<Block> {
    let t = cx.theme;
    let qs = questions(c);
    let out = c.output.as_deref().unwrap_or("");
    if c.status == ToolStatus::Completed && !qs.is_empty() && out.contains("answered") {
        let w = block_width(cx);
        let mut parts: Vec<Rows> = Vec::new();
        let mut rows: Rows = Vec::new();
        for (n, q) in qs.iter().enumerate() {
            if n > 0 {
                rows.push(Vec::new());
            }
            rows.extend(text_rows(q, w, Style::new().fg(t.text_muted)));
            let a = answer_for(out, q)
                .filter(|a| !a.is_empty())
                .unwrap_or_else(|| "(no answer)".into());
            rows.extend(text_rows(&a, w, Style::new().fg(t.text)));
        }
        parts.push(rows);
        return vec![block(
            BlockSpec {
                title: Some("# Questions".into()),
                spinner_title: false,
                children: parts,
                error: None,
                click: None,
            },
            cx,
        )];
    }
    let n = qs.len();
    let text = format!("Asked {n} question{}", if n == 1 { "" } else { "s" });
    vec![inline_block(
        inline(c, cx, "→", text, "Asking questions…", n > 0),
        cx,
    )]
}

// ---- skill ---------------------------------------------------------------------------------

pub fn skill(c: &ToolCall, cx: &RenderCx) -> Vec<Block> {
    let name = str_in(&c.input, &["name"]).unwrap_or("").to_string();
    vec![inline_block(
        inline(
            c,
            cx,
            "→",
            format!("Skill \"{name}\""),
            "Loading skill…",
            !name.is_empty(),
        ),
        cx,
    )]
}

// ---- generic -------------------------------------------------------------------------------

/// MCP tools reach wizard as `server__tool`; opencode joins them with one underscore.
fn tool_label(c: &ToolCall) -> String {
    let n = if c.name.is_empty() {
        c.title.clone()
    } else {
        c.name.replace("__", "_")
    };
    let args = input_args(&c.input, &[]);
    format!("{n} {args}").trim_end().to_string()
}

pub fn generic(c: &ToolCall, cx: &RenderCx) -> Vec<Block> {
    let t = cx.theme;
    let label = tool_label(c);
    let out = clean_output(c.output.as_deref().unwrap_or(""));
    if cx.flags.generic_output && c.status == ToolStatus::Completed && !out.is_empty() {
        let w = block_width(cx);
        let expanded = cx.expanded.contains(&c.id);
        let max_chars = 3 * (cx.width as usize).saturating_sub(6).max(20);
        let (preview, overflow) = collapse(&out, 3, max_chars);
        let shown = if expanded { out.clone() } else { preview };
        let mut inner = text_rows(&shown, w, Style::new().fg(t.text));
        if overflow {
            inner.push(Vec::new());
            inner.extend(click_hint(t, expanded));
        }
        let spec = BlockSpec {
            title: Some(format!("# {label}")),
            spinner_title: false,
            children: vec![inner],
            error: None,
            click: overflow.then(|| c.id.clone()),
        };
        return vec![block(spec, cx)];
    }
    let mut i = inline(c, cx, "⚙", label, "Writing command…", true);
    if denied(c) {
        i.phase = Phase::Denied;
    }
    vec![inline_block(i, cx)]
}
