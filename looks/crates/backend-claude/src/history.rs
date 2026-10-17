//! Session transcripts on disk: replay for `History` and the `Sessions` list.
//!
//! Claude Code keeps one JSONL per session at
//! `<config>/projects/<slug>/<session-id>.jsonl`, where `<config>` is
//! `$CLAUDE_CONFIG_DIR` or `~/.claude` and `<slug>` is the absolute cwd with every
//! character outside `[A-Za-z0-9]` replaced by `-` (so `/home/me/.cfg` becomes
//! `-home-me--cfg`). Slugs longer than 200 chars are cut to 200 and get a hash suffix
//! we do not reproduce; [`project_dirs`] matches those by prefix instead.
//!
//! Besides `user` and `assistant` lines the files hold `attachment`, `queue-operation`,
//! `mode`, `cost-state`, ... which are ignored here. Subagent transcripts live in a
//! sibling `<id>/subagents/` directory, so everything in the main file is main thread
//! apart from lines flagged `isSidechain`.

use crate::tools::{self, apply_result, flatten_content, new_call};
use agent_core::{HistoryItem, SessionInfo};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

pub fn config_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".claude")
}

pub fn slug(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Project directories that can hold sessions for `cwd` (as given and canonicalized).
pub fn project_dirs(cwd: &Path) -> Vec<PathBuf> {
    project_dirs_in(&config_dir().join("projects"), cwd)
}

pub fn project_dirs_in(root: &Path, cwd: &Path) -> Vec<PathBuf> {
    let mut slugs = vec![slug(cwd)];
    if let Ok(real) = cwd.canonicalize() {
        let s = slug(&real);
        if !slugs.contains(&s) {
            slugs.push(s);
        }
    }
    let mut out = Vec::new();
    for s in slugs {
        let direct = root.join(&s);
        if direct.is_dir() {
            out.push(direct);
        } else if s.len() > 200 {
            if let Ok(rd) = std::fs::read_dir(root) {
                out.extend(rd.flatten().map(|e| e.path()).filter(|p| {
                    p.file_name()
                        .map(|n| n.to_string_lossy().starts_with(&s[..200]))
                        .unwrap_or(false)
                }));
            }
        }
    }
    out
}

pub fn session_path(cwd: &Path, id: &str) -> Option<PathBuf> {
    session_path_in(&config_dir().join("projects"), cwd, id)
}

pub fn session_path_in(root: &Path, cwd: &Path, id: &str) -> Option<PathBuf> {
    if id.is_empty() || id.contains('/') || id.contains("..") {
        return None;
    }
    project_dirs_in(root, cwd)
        .into_iter()
        .map(|d| d.join(format!("{id}.jsonl")))
        .find(|p| p.is_file())
}

pub(crate) fn text_of_user(v: &Value) -> Option<String> {
    let c = v.get("message")?.get("content")?;
    let t = match c {
        Value::String(s) => s.clone(),
        Value::Array(bs) => bs
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    Some(t)
}

/// What a user-role text means for the transcript: a real prompt, a slash command the
/// user ran, local command output, or noise the CLI injected.
pub(crate) enum UserText {
    Prompt(String),
    Output(String),
    Skip,
}

pub(crate) fn classify(t: &str) -> UserText {
    let t = t.trim();
    if t.is_empty()
        || t.starts_with("[Request interrupted")
        || t.starts_with("<system-reminder>")
        || t.starts_with("<local-command-caveat>")
    {
        return UserText::Skip;
    }
    if let Some(inner) = t
        .strip_prefix("<local-command-stdout>")
        .and_then(|r| r.strip_suffix("</local-command-stdout>"))
    {
        let inner = inner.trim();
        return if inner.is_empty() {
            UserText::Skip
        } else {
            UserText::Output(inner.to_string())
        };
    }
    if t.starts_with("<command-name>") {
        let tag = |name: &str| -> String {
            let open = format!("<{name}>");
            let close = format!("</{name}>");
            t.split_once(&open)
                .and_then(|(_, r)| r.split_once(&close))
                .map(|(x, _)| x.trim().to_string())
                .unwrap_or_default()
        };
        let line = format!("{} {}", tag("command-name"), tag("command-args"))
            .trim()
            .to_string();
        return UserText::Prompt(line);
    }
    UserText::Prompt(t.to_string())
}

/// Replay of one session, oldest first. Tool results are folded into their calls.
pub fn load(cwd: &Path, id: &str) -> Option<Vec<HistoryItem>> {
    let path = session_path(cwd, id)?;
    let file = File::open(path).ok()?;
    Some(parse(BufReader::new(file), cwd))
}

/// One user message of a session, for rewind. The index in a list of these is the `turn` a
/// frontend asks about, so the list must line up with the `User` items [`parse`] makes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TurnRef {
    /// The CLI's id for the message; `rewind_files` and the transcript line are keyed by it.
    /// Empty for messages the CLI never stored (local commands).
    pub uuid: String,
    pub text: String,
    /// The assistant message that ended the previous turn: `--resume-session-at` cuts there.
    /// `None` when there is nothing to cut back to (a first message) or it is unknown.
    pub prev: Option<String>,
}

/// The user turns of a transcript and the last main-thread assistant message in it.
pub fn turns(r: impl BufRead) -> (Vec<TurnRef>, Option<String>) {
    let mut out = Vec::new();
    let mut last: Option<String> = None;
    for line in r.lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if skipped(&v) {
            continue;
        }
        match v.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                last = v.get("uuid").and_then(Value::as_str).map(str::to_string);
            }
            Some("user") => {
                if let Some(UserText::Prompt(p)) = text_of_user(&v).map(|t| classify(&t)) {
                    out.push(TurnRef {
                        uuid: v
                            .get("uuid")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        text: p,
                        prev: last.clone(),
                    });
                }
            }
            _ => {}
        }
    }
    (out, last)
}

fn skipped(v: &Value) -> bool {
    v.get("isSidechain").and_then(Value::as_bool) == Some(true)
        || v.get("isMeta").and_then(Value::as_bool) == Some(true)
        || v.get("isCompactSummary").and_then(Value::as_bool) == Some(true)
}

/// Replay of a session up to, not including, the user line with `uuid`.
pub fn load_before(cwd: &Path, id: &str, uuid: &str) -> Option<Vec<HistoryItem>> {
    let path = session_path(cwd, id)?;
    let file = File::open(path).ok()?;
    let mut kept = String::new();
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if !uuid.is_empty()
            && serde_json::from_str::<Value>(&line).is_ok_and(|v| {
                v.get("type").and_then(Value::as_str) == Some("user")
                    && v.get("uuid").and_then(Value::as_str) == Some(uuid)
            })
        {
            break;
        }
        kept.push_str(&line);
        kept.push('\n');
    }
    Some(parse(kept.as_bytes(), cwd))
}

pub fn parse(r: impl BufRead, cwd: &Path) -> Vec<HistoryItem> {
    let mut items: Vec<HistoryItem> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    for line in r.lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if skipped(&v) {
            continue;
        }
        match v.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                let Some(blocks) = v
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(Value::as_array)
                else {
                    continue;
                };
                for b in blocks {
                    match b.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(t) = b
                                .get("text")
                                .and_then(Value::as_str)
                                .filter(|t| !t.trim().is_empty())
                            {
                                items.push(HistoryItem::Assistant(t.to_string()));
                            }
                        }
                        Some("thinking") => {
                            if let Some(t) = b
                                .get("thinking")
                                .and_then(Value::as_str)
                                .filter(|t| !t.trim().is_empty())
                            {
                                items.push(HistoryItem::Thought(t.to_string()));
                            }
                        }
                        Some("tool_use") => {
                            let (Some(id), Some(name)) = (
                                b.get("id").and_then(Value::as_str),
                                b.get("name").and_then(Value::as_str),
                            ) else {
                                continue;
                            };
                            let mut call = new_call(
                                id,
                                name,
                                b.get("input").cloned().unwrap_or(Value::Null),
                                cwd,
                                None,
                            );
                            // A result fills this in. One that never comes means the session was
                            // cut off mid-call, which is neither running nor a success.
                            call.status = agent_core::ToolStatus::Failed;
                            call.output = Some(agent_core::INTERRUPTED_OUTPUT.into());
                            at.insert(id.to_string(), items.len());
                            items.push(HistoryItem::Tool(call));
                        }
                        _ => {}
                    }
                }
            }
            Some("user") => {
                let tur = v
                    .get("toolUseResult")
                    .or_else(|| v.get("tool_use_result"))
                    .cloned()
                    .unwrap_or(Value::Null);
                if let Some(blocks) = v
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(Value::as_array)
                {
                    for b in blocks
                        .iter()
                        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
                    {
                        let Some(id) = b.get("tool_use_id").and_then(Value::as_str) else {
                            continue;
                        };
                        let Some(&i) = at.get(id) else { continue };
                        if let HistoryItem::Tool(call) = &mut items[i] {
                            let text = flatten_content(b.get("content").unwrap_or(&Value::Null));
                            apply_result(
                                call,
                                &text,
                                b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                                &tur,
                            );
                            if is_plain_rejection(call) {
                                call.output = Some(agent_core::INTERRUPTED_OUTPUT.into());
                            }
                            // Stored transcripts can be huge; the replay does not need every line.
                            call.output = call.output.take().map(|o| tools::cap(&o, 4000));
                        }
                    }
                }
                if let Some(t) = text_of_user(&v) {
                    match classify(&t) {
                        UserText::Prompt(p) => items.push(HistoryItem::User(p)),
                        UserText::Output(o) => items.push(HistoryItem::Assistant(o)),
                        UserText::Skip => {}
                    }
                }
            }
            _ => {}
        }
    }
    items
}

/// claude writes its refusal text into the result both when you say no in its own prompt and
/// when you interrupt a running tool, and the transcript does not say which. With no note from
/// you in it, calling it a denial would be a guess, so it replays as interrupted. A note
/// ("the user said: ...") can only have come from a refusal and stays one.
fn is_plain_rejection(call: &agent_core::ToolCall) -> bool {
    call.status == agent_core::ToolStatus::Failed
        && call.output.as_deref().is_some_and(|o| {
            o.trim()
                .starts_with("The user doesn't want to proceed with this tool use")
                && !o.contains("the user said:")
        })
}

fn read_chunk(f: &mut File, from: u64, len: usize) -> String {
    let mut buf = vec![0u8; len];
    if f.seek(SeekFrom::Start(from)).is_err() {
        return String::new();
    }
    let n = f.read(&mut buf).unwrap_or(0);
    buf.truncate(n);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Title and first prompt from the ends of a transcript, without reading all of it.
fn summarize(path: &Path) -> Option<(String, bool)> {
    const CHUNK: u64 = 256 * 1024;
    let mut f = File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let head = read_chunk(&mut f, 0, CHUNK as usize);
    let tail = if len > CHUNK {
        read_chunk(&mut f, len - CHUNK, CHUNK as usize)
    } else {
        String::new()
    };
    let mut title: Option<String> = None;
    let mut first_prompt: Option<String> = None;
    let mut any = false;
    for (n, chunk) in [&head, &tail].into_iter().enumerate() {
        let mut lines = chunk.lines();
        if n == 1 {
            lines.next(); // starts mid-line
        }
        for l in lines {
            let Ok(v) = serde_json::from_str::<Value>(l) else {
                continue;
            };
            match v.get("type").and_then(Value::as_str) {
                Some("custom-title") => {
                    title = v
                        .get("customTitle")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or(title)
                }
                // Titles are rewritten as the chat goes on; the tail has the latest.
                Some("ai-title") if title.is_none() || n == 1 => {
                    title = v
                        .get("aiTitle")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or(title);
                }
                Some("user") | Some("assistant") => {
                    any = true;
                    if first_prompt.is_none()
                        && v.get("type").and_then(Value::as_str) == Some("user")
                        && v.get("isMeta").is_none()
                    {
                        if let Some(UserText::Prompt(p)) = text_of_user(&v).map(|t| classify(&t)) {
                            first_prompt = Some(p);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let title = title
        .or(first_prompt)
        .map(|t| one_line(&t, 80))
        .unwrap_or_default();
    Some((title, any))
}

pub(crate) fn one_line(s: &str, max: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        format!("{}...", flat.chars().take(max - 3).collect::<String>())
    }
}

/// Sessions for `cwd`, newest first, capped at `limit`. Files with no conversation in them
/// (a `claude` that was started and closed) are left out.
pub fn list_sessions(cwd: &Path, limit: usize) -> Vec<SessionInfo> {
    list_sessions_in(&config_dir().join("projects"), cwd, limit)
}

pub fn list_sessions_in(root: &Path, cwd: &Path, limit: usize) -> Vec<SessionInfo> {
    let mut found: Vec<SessionInfo> = Vec::new();
    for dir in project_dirs_in(root, cwd) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(id) = p.file_stem().and_then(|s| s.to_str()).map(str::to_string) else {
                continue;
            };
            let updated = e
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            found.push(SessionInfo {
                id,
                title: String::new(),
                cwd: cwd.to_string_lossy().into_owned(),
                updated,
            });
        }
    }
    found.sort_by_key(|s| std::cmp::Reverse(s.updated));
    let mut out = Vec::new();
    for mut s in found {
        if out.len() >= limit {
            break;
        }
        let Some(path) = session_path_in(root, cwd, &s.id) else {
            continue;
        };
        let Some((title, any)) = summarize(&path) else {
            continue;
        };
        if !any {
            continue;
        }
        s.title = if title.is_empty() {
            "(untitled)".into()
        } else {
            title
        };
        out.push(s);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_replaces_every_non_alphanumeric() {
        assert_eq!(
            slug(Path::new("/home/nixos/.config/x_y")),
            "-home-nixos--config-x-y"
        );
        assert_eq!(
            slug(Path::new("/tmp/claude-1000/-home-nixos")),
            "-tmp-claude-1000--home-nixos"
        );
    }

    #[test]
    fn classify_commands_and_noise() {
        assert!(
            matches!(classify("<command-name>/model</command-name>\n<command-args>opus</command-args>"), UserText::Prompt(p) if p == "/model opus")
        );
        assert!(
            matches!(classify("<local-command-stdout>Set model</local-command-stdout>"), UserText::Output(o) if o == "Set model")
        );
        assert!(matches!(
            classify("[Request interrupted by user]"),
            UserText::Skip
        ));
        assert!(matches!(classify("hello"), UserText::Prompt(_)));
    }

    const TWO_TURNS: &str = r#"{"type":"user","uuid":"u1","message":{"role":"user","content":"first"}}
{"type":"assistant","uuid":"a1","message":{"content":[{"type":"thinking","thinking":""}]}}
{"type":"assistant","uuid":"a2","message":{"content":[{"type":"text","text":"one"}]}}
{"type":"attachment","uuid":"x1","parentUuid":"a2"}
{"type":"user","uuid":"u2","message":{"role":"user","content":"second"}}
{"type":"user","uuid":"s1","isSidechain":true,"message":{"role":"user","content":"inner"}}
{"type":"assistant","uuid":"a3","message":{"content":[{"type":"text","text":"two"}]}}
"#;

    #[test]
    fn turns_know_where_the_previous_one_ended() {
        let (t, last) = turns(TWO_TURNS.as_bytes());
        assert_eq!(t.len(), 2, "the sidechain line is not a turn: {t:?}");
        assert_eq!((t[0].uuid.as_str(), t[0].prev.as_deref()), ("u1", None));
        assert_eq!(
            (t[1].uuid.as_str(), t[1].prev.as_deref()),
            ("u2", Some("a2"))
        );
        assert_eq!(t[1].text, "second");
        assert_eq!(last.as_deref(), Some("a3"));
    }

    #[test]
    fn an_unfinished_call_and_an_interrupted_one_replay_as_interrupted_not_success_or_denied() {
        let t = r#"{"type":"user","uuid":"u1","message":{"role":"user","content":"run them"}}
{"type":"assistant","uuid":"a1","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"sleep 321"}}]}}
{"type":"user","uuid":"u2","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":"The user doesn't want to proceed with this tool use. The tool use was rejected (eg. if it was a file edit, the new_string was NOT written to the file). STOP what you are doing and wait for the user to tell you how to proceed."}]}}
{"type":"assistant","uuid":"a2","message":{"content":[{"type":"tool_use","id":"t2","name":"Bash","input":{"command":"sleep 322"}}]}}
"#;
        let items = parse(t.as_bytes(), Path::new("/w"));
        let calls: Vec<&agent_core::ToolCall> = items
            .iter()
            .filter_map(|i| match i {
                HistoryItem::Tool(c) => Some(c),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 2);
        for c in &calls {
            assert_eq!(c.status, agent_core::ToolStatus::Failed, "{c:?}");
            assert_eq!(c.output.as_deref(), Some(agent_core::INTERRUPTED_OUTPUT));
        }
    }

    #[test]
    fn a_refusal_with_your_note_stays_a_denial_and_a_finished_call_a_success() {
        let t = r#"{"type":"assistant","uuid":"a1","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"rm -rf x"}},{"type":"tool_use","id":"t2","name":"Bash","input":{"command":"ls"}}]}}
{"type":"user","uuid":"u2","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":"The user doesn't want to proceed with this tool use. The tool use was rejected. To tell you how to proceed, the user said:\nuse trash instead"},{"type":"tool_result","tool_use_id":"t2","content":"a\nb"}]}}
"#;
        let items = parse(t.as_bytes(), Path::new("/w"));
        let calls: Vec<&agent_core::ToolCall> = items
            .iter()
            .filter_map(|i| match i {
                HistoryItem::Tool(c) => Some(c),
                _ => None,
            })
            .collect();
        assert!(calls[0]
            .output
            .as_deref()
            .unwrap()
            .contains("the user said:"));
        assert_eq!(calls[1].status, agent_core::ToolStatus::Completed);
    }

    #[test]
    fn turn_list_lines_up_with_the_replayed_user_items() {
        let items = parse(TWO_TURNS.as_bytes(), Path::new("/w"));
        let users = items
            .iter()
            .filter(|i| matches!(i, HistoryItem::User(_)))
            .count();
        assert_eq!(users, turns(TWO_TURNS.as_bytes()).0.len());
    }
}
