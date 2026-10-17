//! Pure helpers that turn ACP JSON into `agent-core` values. No I/O, no state.

use agent_core::{
    Config, FileDiff, ModelOption, NoticeLevel, SessionInfo, SlashCommand, StopReason, Todo,
    TodoStatus, ToolCall, ToolKind, ToolStatus, Usage,
};
use serde_json::Value;

pub fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// `xai-oauth/grok-4.6` -> (`xai-oauth`, `grok-4.6`); only the first `/` splits,
/// so `openrouter/anthropic/claude-sonnet-5` keeps its vendor prefix in the name.
pub fn split_model(id: &str) -> (&str, &str) {
    match id.split_once('/') {
        Some((p, n)) => (p, n),
        None => ("", id),
    }
}

fn option_values(opt: &Value) -> Vec<String> {
    opt.get("options")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|o| str_of(o, "value").to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn find_option<'a>(options: &'a [Value], id: &str, category: &str) -> Option<&'a Value> {
    options
        .iter()
        .find(|o| str_of(o, "id") == id)
        .or_else(|| options.iter().find(|o| str_of(o, "category") == category))
}

/// Build the frontend's `Config` from a session's `configOptions`.
pub fn build_config(options: &[Value], cwd: &str, backend: &str) -> Config {
    let mut c = Config {
        cwd: cwd.to_string(),
        backend: backend.to_string(),
        ..Default::default()
    };
    if let Some(m) = find_option(options, "model", "model") {
        c.model = str_of(m, "currentValue").to_string();
        c.models = option_values(m)
            .into_iter()
            .map(|id| {
                let (provider, name) = split_model(&id);
                ModelOption {
                    name: name.to_string(),
                    provider: provider.to_string(),
                    id: id.clone(),
                }
            })
            .collect();
    }
    if let Some(e) = find_option(options, "thought_level", "thought_level") {
        c.effort = str_of(e, "currentValue").to_string();
        c.efforts = option_values(e);
    }
    if let Some(m) = find_option(options, "wizard_mode", "mode") {
        c.mode = str_of(m, "currentValue").to_string();
        c.modes = option_values(m);
    }
    c
}

pub fn parse_commands(update: &Value) -> Vec<SlashCommand> {
    update
        .get("availableCommands")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|c| SlashCommand {
                    name: str_of(c, "name").trim_start_matches('/').to_string(),
                    description: str_of(c, "description").to_string(),
                    input_hint: c
                        .get("input")
                        .map(|i| str_of(i, "hint").to_string())
                        .unwrap_or_default(),
                })
                .filter(|c| !c.name.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// The user's own commands in an `available_commands_update`: wizard marks them in `_meta`,
/// because a prompt naming one is expanded and goes to the model rather than being answered.
pub fn custom_commands(update: &Value) -> Vec<String> {
    update
        .get("availableCommands")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter(|c| c.pointer("/_meta/custom").and_then(Value::as_bool) == Some(true))
                .map(|c| str_of(c, "name").trim_start_matches('/').to_string())
                .collect()
        })
        .unwrap_or_default()
}

pub fn parse_session(s: &Value) -> SessionInfo {
    SessionInfo {
        id: str_of(s, "sessionId").to_string(),
        title: str_of(s, "title").to_string(),
        cwd: str_of(s, "cwd").to_string(),
        updated: parse_rfc3339(str_of(s, "updatedAt")),
    }
}

/// Unix seconds from `2026-10-05T17:21:40.665160287+00:00`; 0 when it does not parse.
pub fn parse_rfc3339(s: &str) -> i64 {
    fn num(s: &str, a: usize, b: usize) -> Option<i64> {
        s.get(a..b)?.parse().ok()
    }
    let parse = || -> Option<i64> {
        let (y, mo, d) = (num(s, 0, 4)?, num(s, 5, 7)?, num(s, 8, 10)?);
        let (h, mi, se) = (num(s, 11, 13)?, num(s, 14, 16)?, num(s, 17, 19)?);
        let mut rest = s.get(19..)?;
        if let Some(frac) = rest.strip_prefix('.') {
            rest = frac.trim_start_matches(|c: char| c.is_ascii_digit());
        }
        let offset = match rest.chars().next() {
            Some('Z') | None => 0,
            Some(sign @ ('+' | '-')) => {
                let secs = num(rest, 1, 3)? * 3600 + num(rest, 4, 6).unwrap_or(0) * 60;
                if sign == '+' {
                    secs
                } else {
                    -secs
                }
            }
            _ => return None,
        };
        Some(days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + se - offset)
    };
    parse().unwrap_or(0)
}

// Howard Hinnant's days_from_civil.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub fn stop_reason(s: &str) -> StopReason {
    match s {
        "cancelled" => StopReason::Cancelled,
        "max_turn_requests" | "max_tokens" => StopReason::MaxTurns,
        "refusal" => StopReason::Error,
        _ => StopReason::EndTurn,
    }
}

/// One line for a JSON-RPC error object: message plus whatever `data` carries.
pub fn rpc_error_text(err: &Value) -> String {
    let msg = str_of(err, "message");
    let data = match err.get("data") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        // structured detail: its own message if it has one, never the raw JSON
        Some(Value::Object(o)) => ["message", "detail", "error", "reason"]
            .iter()
            .find_map(|k| o.get(*k).and_then(Value::as_str))
            .unwrap_or("")
            .to_string(),
        Some(v) => v.to_string(),
    };
    match (msg.is_empty(), data.is_empty()) {
        (_, true) => msg.to_string(),
        (true, false) => data,
        (false, false) => format!("{msg}: {data}"),
    }
}

pub fn is_turn_running_error(err: &Value) -> bool {
    let t = rpc_error_text(err).to_lowercase();
    t.contains("turn is running") || t.contains("between turns")
}

// ---- tools ----------------------------------------------------------------

/// Wizard titles are `name` or `name: detail`; the wire has no separate name field.
pub fn tool_name(wire_title: &str) -> String {
    match wire_title.split_once(": ") {
        Some((n, _)) if !n.contains(char::is_whitespace) => n.to_string(),
        _ if wire_title.contains(char::is_whitespace) => String::new(),
        _ => wire_title.to_string(),
    }
}

/// Short title for the UI: the thing the tool acts on, from rawInput when it
/// has one, else whatever followed `name: ` on the wire.
pub fn tool_title(name: &str, wire_title: &str, raw: &Value) -> String {
    for key in ["command", "pattern", "path", "url", "query"] {
        if let Some(s) = raw.get(key).and_then(Value::as_str) {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    match wire_title
        .strip_prefix(name)
        .and_then(|r| r.strip_prefix(": "))
    {
        Some(rest) => rest.to_string(),
        None if name.is_empty() => wire_title.to_string(),
        None => String::new(),
    }
}

pub fn tool_kind(s: &str) -> ToolKind {
    match s {
        "read" => ToolKind::Read,
        "edit" | "delete" | "move" => ToolKind::Edit,
        "search" => ToolKind::Search,
        "execute" => ToolKind::Execute,
        "fetch" => ToolKind::Fetch,
        "think" => ToolKind::Think,
        _ => ToolKind::Other,
    }
}

pub fn tool_status(s: &str) -> Option<ToolStatus> {
    match s {
        "pending" => Some(ToolStatus::Pending),
        "in_progress" => Some(ToolStatus::Running),
        "completed" => Some(ToolStatus::Completed),
        "failed" => Some(ToolStatus::Failed),
        _ => None,
    }
}

/// Text and (real ACP) diff blocks out of a `content` array.
pub fn tool_content(content: &Value) -> (Option<String>, Option<FileDiff>) {
    let mut texts: Vec<&str> = Vec::new();
    let mut diff = None;
    for item in content.as_array().into_iter().flatten() {
        match str_of(item, "type") {
            "content" => {
                if let Some(inner) = item.get("content") {
                    if str_of(inner, "type") == "text" {
                        texts.push(str_of(inner, "text"));
                    }
                }
            }
            "diff" => {
                diff = Some(FileDiff {
                    path: str_of(item, "path").to_string(),
                    old: item
                        .get("oldText")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    new: str_of(item, "newText").to_string(),
                });
            }
            _ => {}
        }
    }
    let text = if texts.is_empty() {
        None
    } else {
        Some(cap_output(texts.join("\n")))
    };
    (text, diff)
}

/// Most of a tool's output the transcript keeps. The UI re-cleans and re-wraps it on every
/// frame of a running call, and a `cat` of a log can be tens of megabytes.
pub const OUTPUT_CAP: usize = 256 * 1024;
/// The part of the cap taken from the start; the rest comes from the end, where a build's
/// error or a test's summary is.
const OUTPUT_HEAD: usize = 32 * 1024;

/// `s` unchanged when it fits [`OUTPUT_CAP`], else its start and end around a line saying how
/// much came out of the middle.
pub fn cap_output(s: String) -> String {
    if s.len() <= OUTPUT_CAP {
        return s;
    }
    let mut head = OUTPUT_HEAD;
    while !s.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = s.len() - (OUTPUT_CAP - OUTPUT_HEAD);
    while !s.is_char_boundary(tail) {
        tail += 1;
    }
    let lines = s[head..tail].matches('\n').count();
    format!(
        "{}\n[{lines} lines ({} bytes) hidden]\n{}",
        &s[..head],
        tail - head,
        &s[tail..]
    )
}

/// Wizard's edit tools never send a diff block, so rebuild what the arguments
/// say. `edit_file` gives the replaced snippet, not the whole file; `write_file`
/// gives the new content with no old side (an overwrite looks like a new file).
pub fn synth_diff(name: &str, raw: &Value) -> Option<FileDiff> {
    let path = raw.get("path").and_then(Value::as_str)?;
    match name {
        "write_file" => Some(FileDiff {
            path: path.to_string(),
            old: None,
            new: str_of(raw, "content").to_string(),
        }),
        "edit_file" => Some(FileDiff {
            path: path.to_string(),
            old: Some(str_of(raw, "old_string").to_string()),
            new: str_of(raw, "new_string").to_string(),
        }),
        _ => None,
    }
}

/// A `tool_call` update (or its replay twin) as a full `ToolCall`.
pub fn tool_from_update(u: &Value) -> ToolCall {
    let wire_title = str_of(u, "title");
    let name = tool_name(wire_title);
    let input = u.get("rawInput").cloned().unwrap_or(Value::Null);
    let (output, diff) = u.get("content").map(tool_content).unwrap_or((None, None));
    let status = tool_status(str_of(u, "status")).unwrap_or(ToolStatus::Pending);
    let diff = diff.or_else(|| synth_diff(&name, &input));
    ToolCall {
        id: str_of(u, "toolCallId").to_string(),
        kind: tool_kind(str_of(u, "kind")),
        title: tool_title(&name, wire_title, &input),
        name,
        input,
        status,
        output,
        diff,
        parent_id: None,
    }
}

/// Fold a `tool_call_update` into the call it refers to.
pub fn apply_tool_update(call: &mut ToolCall, u: &Value) {
    if let Some(s) = u
        .get("status")
        .and_then(Value::as_str)
        .and_then(tool_status)
    {
        call.status = s;
    }
    if let Some(t) = u.get("title").and_then(Value::as_str) {
        let name = tool_name(t);
        if !name.is_empty() {
            call.name = name;
        }
    }
    if let Some(k) = u.get("kind").and_then(Value::as_str) {
        call.kind = tool_kind(k);
    }
    if let Some(raw) = u.get("rawInput") {
        call.input = raw.clone();
    }
    if let Some(c) = u.get("content") {
        let (text, diff) = tool_content(c);
        if text.is_some() {
            call.output = text;
        }
        if diff.is_some() {
            call.diff = diff;
        }
    }
    if u.get("title").is_some() || u.get("rawInput").is_some() {
        let wire = u.get("title").and_then(Value::as_str).unwrap_or("");
        call.title = tool_title(&call.name, wire, &call.input);
    }
    if call.status == ToolStatus::Failed {
        call.diff = None;
    }
}

fn todo_status(s: &str) -> TodoStatus {
    match s {
        "in_progress" => TodoStatus::InProgress,
        "completed" => TodoStatus::Completed,
        _ => TodoStatus::Pending,
    }
}

/// The list a finished `todo` call leaves behind: its `items` argument on a
/// write, the glyph lines of its output on a read.
pub fn todos_of(call: &ToolCall) -> Option<Vec<Todo>> {
    if call.name != "todo" || call.status != ToolStatus::Completed {
        return None;
    }
    if let Some(items) = call.input.get("items").and_then(Value::as_array) {
        return Some(
            items
                .iter()
                .map(|i| Todo {
                    text: str_of(i, "content").to_string(),
                    status: todo_status(str_of(i, "status")),
                })
                .collect(),
        );
    }
    let out = call.output.as_deref()?;
    if out.trim() == "(todo list is empty)" {
        return Some(Vec::new());
    }
    let list = parse_todo_lines(out);
    if list.is_empty() {
        None
    } else {
        Some(list)
    }
}

/// `✓ done`, `▸ doing`, `☐ later`, as `/todos` and the todo tool print them.
pub fn parse_todo_lines(text: &str) -> Vec<Todo> {
    text.lines()
        .filter_map(|l| {
            let mut ch = l.chars();
            let status = match ch.next()? {
                '✓' => TodoStatus::Completed,
                '▸' => TodoStatus::InProgress,
                '☐' => TodoStatus::Pending,
                _ => return None,
            };
            Some(Todo {
                text: ch.as_str().trim().to_string(),
                status,
            })
        })
        .collect()
}

// ---- usage ----------------------------------------------------------------

/// Parse `/status` or `/cost` text: `usage: 27821 prompt + 196 completion tokens`
/// and `context: 9377 tokens`. ACP carries no usage notification, so this is
/// the only per-session number the server will give.
pub fn parse_usage(text: &str) -> Option<Usage> {
    let mut u = Usage::default();
    let mut found = false;
    for line in text.lines() {
        let toks: Vec<&str> = line.split_whitespace().collect();
        if let Some(i) = toks.iter().position(|t| *t == "prompt") {
            if i >= 1 && toks.get(i + 1) == Some(&"+") {
                if let (Ok(p), Some(Ok(c))) = (
                    toks[i - 1].parse::<u64>(),
                    toks.get(i + 2).map(|t| t.parse::<u64>()),
                ) {
                    u.input_tokens = p;
                    u.output_tokens = c;
                    found = true;
                }
            }
        }
        if toks.first() == Some(&"context:") {
            if let Some(Ok(n)) = toks.get(1).map(|t| t.parse::<u64>()) {
                u.context_tokens = n;
                found = true;
            }
        }
    }
    found.then_some(u)
}

/// Wizard routes its own errors and notices into the message stream as
/// `[wizard] text`; returns the text and a level when a chunk is one of those.
pub fn wizard_notice(chunk: &str) -> Option<(NoticeLevel, String)> {
    let rest = chunk
        .trim_start_matches('\n')
        .strip_prefix("[wizard] ")?
        .trim();
    // `plan mode off` after an approved plan is news like `/plan`'s answer, not a warning
    let level = match rest {
        "plan mode off" => NoticeLevel::Info,
        _ => NoticeLevel::Warn,
    };
    Some((level, rest.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Finding 12: outputs were stored whole, so a 16 MB `cat` cost 48 ms a frame.
    #[test]
    fn a_huge_tool_output_keeps_its_ends_and_says_what_it_dropped() {
        let big: String = (0..200_000).map(|i| format!("line {i}\n")).collect();
        let u = serde_json::json!({
            "toolCallId": "t", "title": "bash: cat log", "kind": "execute", "status": "completed",
            "content": [{"type": "content", "content": {"type": "text", "text": big}}],
        });
        let call = tool_from_update(&u);
        let out = call.output.unwrap();
        assert!(out.len() < OUTPUT_CAP + 100, "{} bytes kept", out.len());
        assert!(out.starts_with("line 0\n"));
        assert!(out.ends_with("line 199999\n"));
        assert!(
            out.contains(" lines (") && out.contains("hidden]"),
            "no marker"
        );
    }

    #[test]
    fn a_cap_never_cuts_inside_a_character() {
        let s = "é".repeat(OUTPUT_CAP);
        let out = cap_output(s);
        assert!(out.starts_with('é') && out.ends_with('é'));
    }

    #[test]
    fn a_small_output_is_untouched() {
        assert_eq!(cap_output("hello\n".into()), "hello\n");
    }
}
