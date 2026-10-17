// OWNER: history-cells (cells, markdown, streaming, tool mapping)
//! History cells (spec Part B) and the `ChatLog` that folds backend events into them.
//!
//! The cells live in `history_cells/`; the exec and patch cells in `exec_cell.rs` and
//! `diff_render.rs`. `ChatLog` follows Codex's chat widget: answer text streams through a
//! newline-gated stream, a command or search that is still running is the in-flight cell shown
//! inside the viewport, and every cell that finishes is handed back to be appended to the
//! history, with a rule after a turn that did work.

mod hooks;
mod messages;
mod notices;
mod tools;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_core::{
    Event, FileDiff, NoticeLevel, StopReason, Todo, TodoStatus, ToolCall, ToolKind, ToolStatus,
};
use ratatui::text::Line;

pub use hooks::{EntryKind, HookCell, HookStatus, event_label, running_line};
pub use messages::{
    AgentMarkdownCell, AgentMessageCell, ReasoningCell, StreamSource, UserCell,
    agent_markdown_lines, extract_first_bold, sanitize_user_text, split_reasoning_summary,
    stream_tail_lines, wrap_with_gutter,
};
pub use notices::{
    EXEC_OUTPUT_NOTE, ErrorCell, FinalMessageSeparator, INTERRUPTED, InfoCell, NoteCell,
    PATCH_SNIPPET_NOTE, WarningCell,
};
pub use tools::{
    McpCell, PlanUpdateCell, ProposedPlanCell, WebSearchCell, format_and_truncate_tool_result,
    format_json_compact, split_mcp_name, truncate_text,
};

use super::diff_render::{Change, PatchCell, new_patch_apply_failure};
use super::exec_cell::{ExecCall, ExecCell, ExecKind, ExecOutput};
use super::{BoxedCell, HistoryCell};
use crate::markdown::stream::AgentStream;
use crate::width::usable_content_width;

/// The cell that is still running, drawn inside the viewport above the bottom pane.
#[derive(Debug)]
enum Active {
    Exec(ExecCell),
    Search(WebSearchCell),
    Mcp(McpCell),
}

impl Active {
    fn lines(&self, width: u16) -> Vec<Line<'static>> {
        match self {
            Active::Exec(c) => c.display_lines(width),
            Active::Search(c) => c.display_lines(width),
            Active::Mcp(c) => c.display_lines(width),
        }
    }
    fn into_cell(self) -> BoxedCell {
        match self {
            Active::Exec(c) => Box::new(c),
            Active::Search(c) => Box::new(c),
            Active::Mcp(c) => Box::new(c),
        }
    }
}

/// What a backend tool call is, for the cell it becomes.
enum ToolClass {
    /// A shell command or an explore tool (`Read`, `List`, `Search` rows).
    Exec(ExecCall),
    /// An edit tool; the change is missing when wizard sent none (a failed call has no diff).
    Edit(Option<Change>, String),
    Plan(Vec<Todo>),
    Search(String),
    Mcp(String, String),
    Spawn(String, String),
    Skip,
}

fn is_quiet_hook(text: &str) -> bool {
    let t = text.trim();
    t.strip_prefix("hook ")
        .and_then(|r| r.split_once(": "))
        .is_some_and(|(id, rest)| {
            event_label(id).is_some()
                && (rest.starts_with("updated args") || rest.starts_with("appended context"))
        })
}

fn str_in<'a>(v: &'a serde_json::Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| v.get(*k).and_then(|x| x.as_str()))
        .filter(|s| !s.is_empty())
}

fn basename(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// A path as the cells show it: relative to the working directory when it is under it.
pub fn display_path(path: &str, cwd: Option<&std::path::Path>) -> String {
    let p = std::path::Path::new(path);
    if let Some(cwd) = cwd
        && let Ok(rest) = p.strip_prefix(cwd)
        && !rest.as_os_str().is_empty()
    {
        return rest.to_string_lossy().into_owned();
    }
    path.to_string()
}

fn todos_of(call: &ToolCall) -> Option<Vec<Todo>> {
    let items = call.input.get("items")?.as_array()?;
    Some(
        items
            .iter()
            .map(|i| Todo {
                text: i
                    .get("content")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                status: match i.get("status").and_then(|x| x.as_str()) {
                    Some("completed") => TodoStatus::Completed,
                    Some("in_progress") => TodoStatus::InProgress,
                    _ => TodoStatus::Pending,
                },
            })
            .collect(),
    )
}

fn classify_tool(call: &ToolCall, cwd: Option<&std::path::Path>) -> ToolClass {
    let name = call.name.to_lowercase();
    let input = &call.input;
    let exec = |command: String, kind: ExecKind| {
        let mut c = ExecCall::new(call.id.clone(), command);
        c.kind = kind;
        ToolClass::Exec(c)
    };
    match name.as_str() {
        "execute" | "bash" | "shell" | "run_command" | "exec_command" => {
            let command = str_in(input, &["command", "cmd"])
                .map(str::to_string)
                .unwrap_or_else(|| call.title.clone());
            return ToolClass::Exec(ExecCall::new(call.id.clone(), command));
        }
        "git_status" | "git_diff" => {
            let command = name.replacen('_', " ", 1);
            return ToolClass::Exec(ExecCall::new(call.id.clone(), command));
        }
        "read_file" | "read" => {
            let path = str_in(input, &["path", "file_path"]).unwrap_or(&call.title);
            let shown = display_path(path, cwd);
            return exec(
                format!("cat {shown}"),
                ExecKind::Read {
                    name: basename(path),
                },
            );
        }
        "list_files" | "ls" | "list" => {
            let path = str_in(input, &["path", "directory"]).unwrap_or(".");
            return exec(
                format!("ls {}", display_path(path, cwd)),
                ExecKind::List {
                    path: Some(display_path(path, cwd)),
                },
            );
        }
        "glob" | "find_files" => {
            let pattern = str_in(input, &["pattern", "glob"]).unwrap_or(&call.title);
            return exec(
                format!("ls {pattern}"),
                ExecKind::List {
                    path: Some(pattern.to_string()),
                },
            );
        }
        "search_files" | "grep" => {
            let query = str_in(input, &["pattern", "query"]).unwrap_or(&call.title);
            let path = str_in(input, &["path"]).map(|p| display_path(p, cwd));
            let command = match &path {
                Some(p) => format!("rg {query} {p}"),
                None => format!("rg {query}"),
            };
            return exec(
                command,
                ExecKind::Search {
                    query: Some(query.to_string()),
                    path,
                },
            );
        }
        "edit_file" | "write_file" | "edit" | "write" | "apply_patch" => {
            if let Some(FileDiff { path, old, new }) = &call.diff {
                let shown = display_path(path, cwd);
                let change = match old {
                    None => Change::Add {
                        content: new.clone(),
                    },
                    Some(old) => Change::Update {
                        old: old.clone(),
                        new: new.clone(),
                        move_to: None,
                    },
                };
                return ToolClass::Edit(Some(change), shown);
            }
            let path = str_in(input, &["path", "file_path"]).unwrap_or("");
            return ToolClass::Edit(None, display_path(path, cwd));
        }
        "web_search" | "x_search" => {
            let q = str_in(input, &["query", "q"]).unwrap_or(&call.title);
            return ToolClass::Search(q.to_string());
        }
        "web_fetch" | "fetch" => {
            let u = str_in(input, &["url"]).unwrap_or(&call.title);
            return ToolClass::Search(u.to_string());
        }
        "todo" | "todowrite" | "todo_write" => {
            return match todos_of(call) {
                Some(t) => ToolClass::Plan(t),
                None => ToolClass::Skip,
            };
        }
        "spawn" | "spawn_subagent" | "subagent" | "task" | "agent" => {
            let label = str_in(input, &["description", "name", "agent", "subagent_type"])
                .unwrap_or("subagent")
                .to_string();
            let prompt = str_in(input, &["prompt", "task", "instructions"])
                .unwrap_or("")
                .to_string();
            return ToolClass::Spawn(label, prompt);
        }
        _ => {}
    }
    if matches!(call.kind, ToolKind::Think) {
        return ToolClass::Skip;
    }
    let (server, tool) =
        split_mcp_name(&call.name).unwrap_or_else(|| (String::new(), call.name.clone()));
    ToolClass::Mcp(server, tool)
}

/// What wizard's `execute` result says in its own words, back to the command's output and exit
/// code: a labelled `stderr:` section, a closing `exit code: N`, and a stand-in sentence for a
/// command that printed nothing.
fn split_exec_result(text: &str, failed: bool) -> (String, i32) {
    let mut body = text.trim_end_matches('\n').to_string();
    let mut code = i32::from(failed);
    if let Some((head, tail)) = body.rsplit_once("exit code: ")
        && let Ok(n) = tail.trim().parse::<i32>()
        && (head.is_empty() || head.ends_with('\n'))
    {
        code = n;
        body = head.trim_end_matches('\n').to_string();
    }
    if body == "(command succeeded with no output)" {
        body.clear();
    }
    let body = if let Some(rest) = body.strip_prefix("stderr:\n") {
        rest.to_string()
    } else {
        body.replacen("\nstderr:\n", "\n", 1)
    };
    (body, code)
}

/// A subagent started: the verb and its prompt (spec B.14).
#[derive(Clone, Debug)]
struct SpawnCell {
    label: String,
    prompt: String,
}

impl HistoryCell for SpawnCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        use ratatui::style::Stylize;
        let mut out = vec![Line::from(vec![
            "• ".dim(),
            "Spawned ".bold(),
            self.label.clone().cyan().bold(),
        ])];
        if !self.prompt.is_empty() {
            let prompt = truncate_text(&self.prompt, 160);
            let opts = crate::wrap::WrapOpts::new((width as usize).saturating_sub(4).max(1))
                .subsequent_indent(Line::from("    "));
            let wrapped = crate::wrap::adaptive_wrap_line(&Line::from(prompt), &opts);
            out.extend(crate::line_utils::prefix_lines(
                wrapped,
                "  └ ".dim(),
                "    ".into(),
            ));
        }
        out
    }
}

/// Folds `agent_core::Event`s into committed cells. `apply` returns the cells to append to the
/// history; the app writes them into scrollback.
#[derive(Debug)]
pub struct ChatLog {
    width: u16,
    cwd: Option<PathBuf>,
    replay: bool,
    stream: AgentStream,
    stream_source: StreamSource,
    stream_emitted: bool,
    /// Replay keeps a whole answer here instead of streaming it.
    replay_text: String,
    thought: String,
    header: Option<String>,
    active: Option<Active>,
    started: HashMap<String, Instant>,
    done: std::collections::HashSet<String>,
    /// The turn did work (a command, an edit, a tool call): it ends with a rule.
    turn_work: bool,
    /// Work happened since the last rule: the next answer starts below one.
    rule_before_answer: bool,
    turn_started: Option<Instant>,
    /// Answer text has reached the history: the status row stays out of the way until a tool
    /// call or new reasoning starts.
    text_shown: bool,
    /// Text held back at the start of an answer while it could still turn into `<proposed_plan>`.
    head: String,
    /// The plan being collected between `<proposed_plan>` and its closing tag.
    plan: Option<String>,
    /// Say once per session what wizard cannot do (no streamed output, snippet diffs). On unless
    /// `CODEXW_NOTES=0`, which the capture comparisons use.
    pub notes: bool,
    exec_note_shown: bool,
    patch_note_shown: bool,
    /// Edits already drawn above an approval prompt: the finished call must not draw them again.
    previewed_edits: Vec<FileDiff>,
    /// The last answer ended with a held-back table: Codex rebuilds the whole scrollback from
    /// source before going on, so the table is laid out as the finished message.
    reflow: bool,
}

impl Default for ChatLog {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatLog {
    pub fn new() -> Self {
        let width = 80;
        Self {
            width,
            cwd: std::env::current_dir().ok(),
            replay: false,
            stream: AgentStream::new(
                usable_content_width(width as usize, 2),
                std::env::current_dir().ok(),
            ),
            stream_source: Arc::new(Mutex::new(None)),
            stream_emitted: false,
            replay_text: String::new(),
            thought: String::new(),
            header: None,
            active: None,
            started: HashMap::new(),
            done: Default::default(),
            turn_work: false,
            rule_before_answer: false,
            turn_started: None,
            text_shown: false,
            head: String::new(),
            plan: None,
            notes: std::env::var("CODEXW_NOTES").map_or(true, |v| v != "0"),
            exec_note_shown: false,
            patch_note_shown: false,
            previewed_edits: Vec::new(),
            reflow: false,
        }
    }

    /// A log that builds the cells of a loaded session: answers arrive whole and become
    /// source-backed cells, with no stream in between.
    pub fn new_replay() -> Self {
        Self {
            replay: true,
            ..Self::new()
        }
    }

    /// Terminal width and working directory; the app tells the log before each event.
    /// An edit waits for approval: Codex has already drawn what it would change (the `Edited`
    /// cell) above the prompt, so the diff is on screen while the user decides. The finished
    /// call that follows is then not drawn a second time.
    pub fn permission_preview(&mut self, req: &agent_core::PermissionRequest) -> Vec<BoxedCell> {
        let mut out = Vec::new();
        let Some(diff) = req.diff.as_ref() else {
            return out;
        };
        if req.kind != ToolKind::Edit {
            return out;
        }
        self.flush_all(&mut out);
        let change = match &diff.old {
            None => Change::Add {
                content: diff.new.clone(),
            },
            Some(old) => Change::Update {
                old: old.clone(),
                new: diff.new.clone(),
                move_to: None,
            },
        };
        let shown = display_path(&diff.path, self.cwd.as_deref());
        out.push(Box::new(PatchCell::new(vec![(shown, change)])));
        self.previewed_edits.push(diff.clone());
        out
    }

    pub fn set_context(&mut self, width: u16, cwd: &std::path::Path) {
        self.width = width;
        if self.cwd.as_deref() != Some(cwd) {
            self.cwd = Some(cwd.to_path_buf());
            self.stream.set_cwd(self.cwd.clone());
        }
        self.stream
            .set_width(usable_content_width(width as usize, 2));
    }

    fn animations(&self) -> bool {
        !self.replay
    }

    // ---- views for the app --------------------------------------------------------------------

    /// Rows of the in-flight cell drawn inside the viewport: a running command or search, or the
    /// table that a stream is still holding back.
    pub fn active_lines(&self, width: u16) -> Vec<Line<'static>> {
        let tail = self.stream.tail();
        if !tail.is_empty() {
            return stream_tail_lines(tail, self.stream.starts_stream());
        }
        match &self.active {
            Some(a) => a.lines(width),
            None => Vec::new(),
        }
    }

    /// Header for the status row: the first `**bold**` of the reasoning summary, `None` for
    /// `Working` (spec B.16.1).
    pub fn status_header(&self) -> Option<String> {
        self.header.clone()
    }

    /// True while answer text is being committed, which hides the status row.
    pub fn text_streaming(&self) -> bool {
        self.text_shown || self.stream.has_visible_output()
    }

    /// True once after a finished answer that had a live table tail: the app re-emits the
    /// scrollback from its cells (spec B.4.1, Codex's `ConsolidationScrollbackReflow::Required`).
    pub fn take_reflow(&mut self) -> bool {
        std::mem::take(&mut self.reflow)
    }

    // ---- flushing -----------------------------------------------------------------------------

    fn push_stream_lines(&mut self, lines: Vec<Line<'static>>, out: &mut Vec<BoxedCell>) {
        if lines.is_empty() {
            return;
        }
        let first = !self.stream_emitted;
        self.stream_emitted = true;
        self.text_shown = true;
        out.push(Box::new(AgentMessageCell::new(
            lines,
            first,
            self.stream_source.clone(),
            self.cwd.clone(),
        )));
    }

    /// End of an answer: the rows still waiting, and the source for resize.
    fn finalize_stream(&mut self, out: &mut Vec<BoxedCell>) {
        if self.replay {
            if !self.replay_text.trim().is_empty() {
                out.push(Box::new(AgentMarkdownCell {
                    source: std::mem::take(&mut self.replay_text),
                    cwd: self.cwd.clone(),
                }));
            }
            self.replay_text.clear();
            return;
        }
        self.drain_held_text(out);
        if self.stream.is_empty() {
            return;
        }
        if !self.stream.tail().is_empty() {
            self.reflow = true;
        }
        let (rest, source) = self.stream.finalize();
        self.push_stream_lines(rest, out);
        if let Ok(mut slot) = self.stream_source.lock() {
            *slot = Some(source);
        }
        self.stream_source = Arc::new(Mutex::new(None));
        self.stream_emitted = false;
    }

    pub fn flush_thought(&mut self) -> Vec<BoxedCell> {
        let mut out: Vec<BoxedCell> = Vec::new();
        self.flush_thought_into(&mut out);
        out
    }

    fn flush_thought_into(&mut self, out: &mut Vec<BoxedCell>) {
        let text = std::mem::take(&mut self.thought);
        if text.trim().is_empty() {
            return;
        }
        let cell = ReasoningCell::new(&text);
        if !cell.is_empty() {
            out.push(Box::new(cell));
        }
    }

    fn flush_active(&mut self, out: &mut Vec<BoxedCell>) {
        if let Some(a) = self.active.take() {
            out.push(a.into_cell());
        }
    }

    /// Everything in flight goes to the history, in order, before another cell does.
    fn flush_all(&mut self, out: &mut Vec<BoxedCell>) {
        self.flush_thought_into(out);
        self.finalize_stream(out);
        self.flush_active(out);
    }

    fn mark_work(&mut self) {
        self.turn_work = true;
        self.rule_before_answer = true;
    }

    // ---- events -------------------------------------------------------------------------------

    pub fn apply(&mut self, ev: &Event) -> Vec<BoxedCell> {
        let mut out: Vec<BoxedCell> = Vec::new();
        match ev {
            Event::TurnStart => {
                self.header = None;
                self.text_shown = false;
                self.turn_work = false;
                self.rule_before_answer = false;
                self.turn_started = Some(Instant::now());
            }
            Event::TextDelta(d) => {
                self.flush_thought_into(&mut out);
                self.flush_active(&mut out);
                if self.replay {
                    self.replay_text.push_str(d);
                } else {
                    self.feed_text(d, &mut out);
                }
            }
            Event::ThoughtDelta(d) => {
                self.finalize_stream(&mut out);
                self.text_shown = false;
                self.thought.push_str(d);
                if let Some(h) = extract_first_bold(&self.thought) {
                    self.header = Some(h);
                }
            }
            Event::Tool(call) => self.on_tool(call, &mut out),
            Event::TurnEnd(reason) => {
                self.flush_thought_into(&mut out);
                self.finalize_stream(&mut out);
                let cancelled = matches!(reason, StopReason::Cancelled);
                if cancelled {
                    match self.active.as_mut() {
                        Some(Active::Exec(c)) => c.mark_failed(),
                        Some(Active::Mcp(c)) => c.mark_failed(),
                        Some(Active::Search(c)) => c.completed = true,
                        None => {}
                    }
                }
                self.flush_active(&mut out);
                if cancelled {
                    out.push(Box::new(ErrorCell {
                        text: INTERRUPTED.into(),
                    }));
                } else if self.turn_work
                    && matches!(reason, StopReason::EndTurn | StopReason::MaxTurns)
                {
                    let elapsed = self.turn_started.map(|t| t.elapsed().as_secs());
                    out.push(Box::new(FinalMessageSeparator {
                        elapsed_seconds: elapsed,
                    }));
                }
                self.header = None;
                self.text_shown = false;
                self.turn_work = false;
                self.rule_before_answer = false;
                self.turn_started = None;
                self.started.clear();
            }
            Event::Notice { text, .. } if HookCell::from_notice(text).is_some() => {
                self.flush_all(&mut out);
                out.extend(HookCell::from_notice(text).map(|c| Box::new(c) as BoxedCell));
            }
            // A quiet hook outcome (arguments rewritten, context appended) leaves no cell.
            Event::Notice { text, .. }
                if text.trim().starts_with("hook ") && is_quiet_hook(text) => {}
            Event::Notice { level, text } => {
                self.flush_all(&mut out);
                out.push(match level {
                    NoticeLevel::Info => Box::new(InfoCell {
                        text: text.clone(),
                        hint: None,
                    }),
                    NoticeLevel::Warn => Box::new(WarningCell { text: text.clone() }),
                    NoticeLevel::Error => Box::new(ErrorCell { text: text.clone() }),
                });
            }
            Event::Fatal(msg) => {
                self.flush_all(&mut out);
                out.push(Box::new(ErrorCell { text: msg.clone() }));
            }
            _ => {}
        }
        out
    }

    const PLAN_OPEN: &'static str = "<proposed_plan>";
    const PLAN_CLOSE: &'static str = "</proposed_plan>";

    /// Answer text goes to the stream, except a leading `<proposed_plan>` block, which is held
    /// whole and shown as one cell when it closes (spec B.7.2).
    fn feed_text(&mut self, delta: &str, out: &mut Vec<BoxedCell>) {
        let mut text = delta.to_string();
        loop {
            if let Some(buf) = self.plan.as_mut() {
                buf.push_str(&text);
                let Some(end) = buf.find(Self::PLAN_CLOSE) else {
                    return;
                };
                let all = std::mem::take(buf);
                self.plan = None;
                let body = all[..end].trim_matches('\n').to_string();
                text = all[end + Self::PLAN_CLOSE.len()..].to_string();
                self.emit_plan(body, out);
                if text.trim().is_empty() {
                    return;
                }
                continue;
            }
            if self.stream.is_empty() {
                let probe = format!("{}{}", self.head, text);
                let t = probe.trim_start();
                if t.is_empty()
                    || (t.len() < Self::PLAN_OPEN.len() && Self::PLAN_OPEN.starts_with(t))
                {
                    self.head = probe;
                    return;
                }
                self.head.clear();
                if let Some(rest) = t.strip_prefix(Self::PLAN_OPEN) {
                    self.plan = Some(rest.to_string());
                    text = String::new();
                    continue;
                }
                text = probe;
            }
            if self.stream.is_empty() && self.rule_before_answer {
                out.push(Box::new(FinalMessageSeparator {
                    elapsed_seconds: None,
                }));
                self.rule_before_answer = false;
            }
            let lines = self.stream.push(&text);
            self.push_stream_lines(lines, out);
            return;
        }
    }

    fn emit_plan(&mut self, markdown: String, out: &mut Vec<BoxedCell>) {
        self.flush_thought_into(out);
        self.finalize_stream(out);
        self.flush_active(out);
        out.push(Box::new(ProposedPlanCell {
            markdown,
            cwd: self.cwd.clone(),
        }));
        self.text_shown = true;
    }

    /// End of an answer: text still held back, or a plan that never closed.
    fn drain_held_text(&mut self, out: &mut Vec<BoxedCell>) {
        if let Some(buf) = self.plan.take() {
            self.emit_plan(buf.trim_matches('\n').to_string(), out);
        }
        if !self.head.is_empty() {
            let held = std::mem::take(&mut self.head);
            if !held.trim().is_empty() {
                let lines = self.stream.push(&held);
                self.push_stream_lines(lines, out);
            }
        }
    }

    fn on_tool(&mut self, call: &ToolCall, out: &mut Vec<BoxedCell>) {
        if call.parent_id.is_some() || self.done.contains(&call.id) {
            return;
        }
        let finished = matches!(call.status, ToolStatus::Completed | ToolStatus::Failed);
        let failed = call.status == ToolStatus::Failed;
        let first_sight = !self.started.contains_key(&call.id);
        let class = classify_tool(call, self.cwd.as_deref());
        match class {
            ToolClass::Skip => {
                if finished {
                    self.done.insert(call.id.clone());
                }
            }
            ToolClass::Exec(exec) => {
                if first_sight {
                    self.flush_thought_into(out);
                    self.finalize_stream(out);
                    self.text_shown = false;
                    self.started.insert(call.id.clone(), exec.started);
                    self.mark_work();
                    let appended = match self.active.as_mut() {
                        Some(Active::Exec(cell)) => cell.push_call(exec.clone()),
                        _ => false,
                    };
                    if !appended {
                        self.flush_active(out);
                        let mut cell = ExecCell::new(exec);
                        cell.animations = self.animations();
                        self.active = Some(Active::Exec(cell));
                    }
                }
                if finished {
                    self.done.insert(call.id.clone());
                    let duration = self
                        .started
                        .get(&call.id)
                        .map(|t| t.elapsed())
                        .unwrap_or(Duration::ZERO);
                    let raw = call.output.clone().unwrap_or_default();
                    let (text, exit_code) = if call.name.eq_ignore_ascii_case("execute") {
                        split_exec_result(&raw, failed)
                    } else {
                        (raw, i32::from(failed))
                    };
                    let output = ExecOutput { text, exit_code };
                    let mut flush = false;
                    if let Some(Active::Exec(cell)) = self.active.as_mut()
                        && cell.complete_call(&call.id, output, duration)
                    {
                        flush = cell.should_flush();
                    }
                    if flush {
                        self.flush_active(out);
                        if self.notes && !self.replay && !self.exec_note_shown {
                            self.exec_note_shown = true;
                            out.push(Box::new(NoteCell {
                                text: EXEC_OUTPUT_NOTE.into(),
                            }));
                        }
                    }
                }
            }
            ToolClass::Edit(change, path) => {
                if first_sight {
                    self.started.insert(call.id.clone(), Instant::now());
                    self.mark_work();
                }
                if finished {
                    self.done.insert(call.id.clone());
                    self.flush_all(out);
                    let previewed = call
                        .diff
                        .as_ref()
                        .and_then(|d| self.previewed_edits.iter().position(|p| p == d));
                    if let Some(i) = previewed {
                        // Already on screen above the approval prompt.
                        self.previewed_edits.remove(i);
                        if failed {
                            out.push(Box::new(new_patch_apply_failure(
                                call.output.as_deref().unwrap_or(""),
                            )));
                        }
                        return;
                    }
                    match change {
                        Some(change) if !failed => {
                            out.push(Box::new(PatchCell::new(vec![(path, change)])));
                            let snippet = !matches!(call.diff.as_ref(), Some(d) if d.old.is_none());
                            if self.notes && !self.replay && snippet && !self.patch_note_shown {
                                self.patch_note_shown = true;
                                out.push(Box::new(NoteCell {
                                    text: PATCH_SNIPPET_NOTE.into(),
                                }));
                            }
                        }
                        _ if failed => out.push(Box::new(new_patch_apply_failure(
                            call.output.as_deref().unwrap_or(""),
                        ))),
                        _ => {}
                    }
                }
            }
            ToolClass::Plan(steps) => {
                if finished && !failed {
                    self.done.insert(call.id.clone());
                    self.flush_all(out);
                    out.push(Box::new(PlanUpdateCell {
                        explanation: None,
                        steps,
                    }));
                }
            }
            ToolClass::Search(detail) => {
                if first_sight {
                    self.flush_all(out);
                    self.started.insert(call.id.clone(), Instant::now());
                    self.mark_work();
                    let mut cell = WebSearchCell::new(call.id.clone(), detail.clone());
                    cell.animations = self.animations();
                    self.active = Some(Active::Search(cell));
                }
                if finished {
                    self.done.insert(call.id.clone());
                    // Codex commits the `Searching the web` cell as it stands and prints the
                    // result as a second cell; the capture shows both.
                    self.flush_active(out);
                    let mut cell = WebSearchCell::new(call.id.clone(), detail);
                    cell.completed = true;
                    out.push(Box::new(cell));
                }
            }
            ToolClass::Mcp(server, tool) => {
                if first_sight {
                    self.flush_all(out);
                    self.started.insert(call.id.clone(), Instant::now());
                    self.mark_work();
                    let mut cell = McpCell::new(
                        call.id.clone(),
                        server.clone(),
                        tool.clone(),
                        call.input.clone(),
                    );
                    cell.animations = self.animations();
                    self.active = Some(Active::Mcp(cell));
                }
                if finished {
                    self.done.insert(call.id.clone());
                    let mut cell = match self.active.take() {
                        Some(Active::Mcp(c)) if c.id == call.id => c,
                        other => {
                            self.active = other;
                            self.flush_active(out);
                            McpCell::new(call.id.clone(), server, tool, call.input.clone())
                        }
                    };
                    let text = call.output.clone().unwrap_or_default();
                    cell.complete(if failed { Err(text) } else { Ok(text) });
                    out.push(Box::new(cell));
                }
            }
            ToolClass::Spawn(label, prompt) => {
                if first_sight {
                    self.flush_all(out);
                    self.started.insert(call.id.clone(), Instant::now());
                    self.mark_work();
                    out.push(Box::new(SpawnCell { label, prompt }));
                }
                if finished {
                    self.done.insert(call.id.clone());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
