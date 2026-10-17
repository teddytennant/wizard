//! The ACP client as a state machine with no I/O.
//!
//! Feed it `Request`s from the frontend and raw lines from `wizard acp`; it
//! returns [`Action`]s: JSON to write, `Event`s to deliver, or "close stdin".
//! `lib.rs` wires it to a child process; the fixture tests drive it directly.

use std::collections::{HashMap, VecDeque};

use agent_core::{
    Config, Event, HistoryItem, NoticeLevel, PermissionRequest, Request, SessionInfo, SlashCommand,
    StopReason, Todo, ToolCall,
};
use serde_json::{json, Value};

use crate::wire::{self, str_of};

/// `session/list` pages followed before the listing is cut and the user is told. Wizard pages 50
/// at a time, so this is 20,000 sessions.
const MAX_LIST_PAGES: u16 = 400;

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// One JSON-RPC message to write to the child's stdin.
    Send(Value),
    Emit(Event),
    /// Close the child's stdin (graceful shutdown).
    CloseStdin,
}

enum Pending {
    Initialize,
    NewSession {
        startup: bool,
    },
    LoadSession {
        id: String,
        startup: bool,
    },
    List {
        acc: Vec<SessionInfo>,
        pages: u16,
    },
    SetConfig,
    Prompt,
    /// Hidden `/status` run after a turn, only to read the usage lines.
    Probe,
}

struct Turn {
    /// What was typed. Whether it is an advertised command is decided when
    /// output arrives, not when it is sent: a prompt queued during startup goes
    /// out before the commands list has been read, but the server writes that
    /// list before it reads the prompt, so it is in by the first chunk.
    text: String,
    /// Held-back message text of a command turn.
    buf: String,
}

#[derive(PartialEq, Clone, Copy)]
enum ReplayKind {
    User,
    Assistant,
    Thought,
}

#[derive(Default)]
struct Replay {
    id: String,
    items: Vec<HistoryItem>,
    cur: Option<(ReplayKind, String)>,
    tool_idx: HashMap<String, usize>,
    todos: Option<Vec<Todo>>,
}

impl Replay {
    fn text(&mut self, kind: ReplayKind, text: &str) {
        match &mut self.cur {
            // wizard replays each user message as one chunk, so a second one is a second
            // message (an interrupted turn leaves two in a row), not more of the first
            Some((k, buf)) if *k == kind && kind != ReplayKind::User => buf.push_str(text),
            _ => {
                self.flush();
                self.cur = Some((kind, text.to_string()));
            }
        }
    }

    fn flush(&mut self) {
        if let Some((kind, buf)) = self.cur.take() {
            if buf.is_empty() {
                return;
            }
            self.items.push(match kind {
                ReplayKind::User => HistoryItem::User(buf),
                ReplayKind::Assistant => HistoryItem::Assistant(buf),
                ReplayKind::Thought => HistoryItem::Thought(buf),
            });
        }
    }
}

pub struct Core {
    cwd: String,
    resume: Option<String>,
    backend: String,
    next_id: u64,
    pending: HashMap<u64, Pending>,
    session: Option<String>,
    options: Vec<Value>,
    commands: Vec<SlashCommand>,
    /// The user's own commands among them: a prompt naming one is a model turn.
    custom: Vec<String>,
    tools: HashMap<String, ToolCall>,
    turn: Option<Turn>,
    probe: Option<String>,
    /// A session switch asked for while the hidden usage probe ran: made once it ends.
    /// `Some(None)` is a new session.
    switch_after_probe: Option<Option<String>>,
    /// The frontend wants a `/name` nothing runs answered as unknown (it says so to wizard in
    /// `initialize`); such a prompt is then a command turn too.
    answer_unknown: bool,
    /// A session was loaded and its context size has not been read yet.
    probe_after_load: bool,
    replay: Option<Replay>,
    queue: VecDeque<String>,
    session_cwds: HashMap<String, String>,
    /// Put wizard's questions to the frontend instead of answering them here.
    ask: bool,
    /// Questions waiting on the frontend: our id, then wizard's request id and its options.
    asks: HashMap<String, (Value, Vec<Value>)>,
    out: Vec<Action>,
}

impl Core {
    /// `backend` is the footer string (`wizard 3.7.1`); empty means "take it
    /// from the initialize answer".
    pub fn new(cwd: String, resume: Option<String>, backend: String) -> Self {
        Core {
            cwd,
            resume,
            backend,
            next_id: 1,
            pending: HashMap::new(),
            session: None,
            options: Vec::new(),
            commands: Vec::new(),
            custom: Vec::new(),
            tools: HashMap::new(),
            turn: None,
            probe: None,
            switch_after_probe: None,
            answer_unknown: false,
            probe_after_load: false,
            replay: None,
            queue: VecDeque::new(),
            session_cwds: HashMap::new(),
            ask: false,
            asks: HashMap::new(),
            out: Vec::new(),
        }
    }

    /// A turn (not the hidden `/status` probe) is in flight.
    pub fn turn_active(&self) -> bool {
        self.turn.is_some()
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session.as_deref()
    }

    /// Id of the hidden `/status` request while it is in flight. Lets a test
    /// harness answer it; the frontend never sees it.
    pub fn probe_request_id(&self) -> Option<u64> {
        self.pending
            .iter()
            .find(|(_, p)| matches!(p, Pending::Probe))
            .map(|(id, _)| *id)
    }

    /// Show wizard's `session/request_permission` (a finished plan) to the frontend as
    /// [`Event::Permission`] and answer with its [`Request::Decide`]. Off, the core answers
    /// with the first allow option, for frontends that have no prompt to show.
    pub fn set_asking(&mut self, ask: bool) {
        self.ask = ask;
    }

    /// Ask wizard to answer a `/name` nothing runs as unknown, instead of sending it to the
    /// model.
    pub fn set_answer_unknown(&mut self, on: bool) {
        self.answer_unknown = on;
    }

    pub fn config(&self) -> Config {
        wire::build_config(&self.options, &self.cwd, &self.backend)
    }

    fn take(&mut self) -> Vec<Action> {
        std::mem::take(&mut self.out)
    }

    fn emit(&mut self, e: Event) {
        self.out.push(Action::Emit(e));
    }

    fn notice(&mut self, level: NoticeLevel, text: impl Into<String>) {
        self.emit(Event::Notice {
            level,
            text: text.into(),
        });
    }

    fn fatal(&mut self, text: impl Into<String>) {
        self.emit(Event::Fatal(text.into()));
    }

    fn request(&mut self, method: &str, params: Value, p: Pending) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.pending.insert(id, p);
        self.out.push(Action::Send(
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
        ));
        id
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.out.push(Action::Send(
            json!({"jsonrpc": "2.0", "method": method, "params": params}),
        ));
    }

    // ---- startup ----------------------------------------------------------

    pub fn start(&mut self) -> Vec<Action> {
        self.request(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {},
                "clientInfo": {"name": "openw", "version": env!("CARGO_PKG_VERSION")},
                "_meta": {"wizard": {"unknownCommands": self.answer_unknown}},
            }),
            Pending::Initialize,
        );
        self.take()
    }

    fn open_new(&mut self, startup: bool) {
        let cwd = self.cwd.clone();
        self.request(
            "session/new",
            json!({"cwd": cwd, "mcpServers": []}),
            Pending::NewSession { startup },
        );
    }

    fn open_load(&mut self, id: String, startup: bool) {
        let cwd = self
            .session_cwds
            .get(&id)
            .cloned()
            .unwrap_or_else(|| self.cwd.clone());
        self.replay = Some(Replay {
            id: id.clone(),
            ..Default::default()
        });
        self.request(
            "session/load",
            json!({"sessionId": id, "cwd": cwd, "mcpServers": []}),
            Pending::LoadSession { id, startup },
        );
    }

    // ---- requests from the frontend ----------------------------------------

    fn opening(&self) -> bool {
        self.pending
            .values()
            .any(|p| matches!(p, Pending::NewSession { .. } | Pending::LoadSession { .. }))
    }

    fn idle(&self) -> bool {
        self.session.is_some() && self.turn.is_none() && self.probe.is_none() && !self.opening()
    }

    pub fn on_request(&mut self, r: Request) -> Vec<Action> {
        match r {
            Request::Prompt(text) | Request::PromptWith { text, .. } => {
                if !text.trim().is_empty() {
                    // `wizard acp` never answers a prompt sent while a turn runs,
                    // so hold it until the turn is over.
                    self.queue.push_back(text);
                    self.pump();
                }
            }
            Request::Cancel => {
                // ACP: a cancel answers every open question as cancelled
                for (_, (rpc, _)) in std::mem::take(&mut self.asks) {
                    self.answer_permission(rpc, None);
                }
                // those prompts are already in the transcript, so say they will not be answered
                let dropped = std::mem::take(&mut self.queue).len();
                if dropped > 0 {
                    self.notice(
                        NoticeLevel::Info,
                        format!(
                            "Interrupted; {dropped} queued message{} not sent.",
                            if dropped == 1 { " was" } else { "s were" }
                        ),
                    );
                }
                if self.turn.is_some() {
                    if let Some(sid) = self.session.clone() {
                        self.notify("session/cancel", json!({"sessionId": sid}));
                    }
                }
            }
            Request::NewSession => {
                if self.busy_for_switch(None) {
                    return self.take();
                }
                self.open_new(false);
            }
            Request::LoadSession(id) => {
                if self.busy_for_switch(Some(id.clone())) {
                    return self.take();
                }
                self.open_load(id, false);
            }
            Request::ListSessions => {
                let cwd = self.cwd.clone();
                self.request(
                    "session/list",
                    json!({"cwd": cwd}),
                    Pending::List {
                        acc: Vec::new(),
                        pages: 0,
                    },
                );
            }
            Request::SetModel(v) => {
                let v = self.resolve_model(&v);
                self.set_option("model", v);
            }
            Request::SetEffort(v) => self.set_option("thought_level", v),
            Request::SetMode(v) => self.set_option("wizard_mode", v),
            Request::Refresh => {
                if !self.commands.is_empty() {
                    let c = self.commands.clone();
                    self.emit(Event::Commands(c));
                }
                if self.session.is_some() {
                    let c = self.config();
                    self.emit(Event::ConfigChanged(c));
                }
            }
            Request::Shutdown => self.out.push(Action::CloseStdin),
            Request::Decide { id, allow, .. } => {
                if let Some((rpc, options)) = self.asks.remove(&id) {
                    let want = if allow { "allow" } else { "reject" };
                    let pick = options
                        .iter()
                        .find(|o| str_of(o, "kind").starts_with(want))
                        .map(|o| str_of(o, "optionId").to_string());
                    self.answer_permission(rpc, pick);
                }
            }
            // Added to agent-core for openc; wizard has no steering yet.
            Request::Steer(_)
            | Request::Answer { .. }
            | Request::RewindPreview { .. }
            | Request::Rewind { .. } => {}
        }
        self.take()
    }

    fn busy_for_switch(&mut self, target: Option<String>) -> bool {
        // the hidden usage probe is ours, not the user's turn: the switch waits for it
        if self.probe.is_some() && self.turn.is_none() && !self.opening() {
            self.switch_after_probe = Some(target);
            return true;
        }
        if self.turn.is_some() || self.probe.is_some() || self.opening() {
            self.notice(
                NoticeLevel::Warn,
                "Finish or cancel the running turn before switching sessions.",
            );
            return true;
        }
        false
    }

    /// A bare model tag (`grok-4.6`) is resolved against the advertised ids.
    fn resolve_model(&self, v: &str) -> String {
        if v.contains('/') {
            return v.to_string();
        }
        let cfg = self.config();
        let mut hits = cfg.models.iter().filter(|m| m.name == v);
        match (hits.next(), hits.next()) {
            (Some(m), None) => m.id.clone(),
            // Several providers carry it: prefer the one already in use.
            (Some(_), Some(_)) => {
                let (cur, _) = wire::split_model(&cfg.model);
                cfg.models
                    .iter()
                    .find(|m| m.name == v && m.provider == cur)
                    .map(|m| m.id.clone())
                    .unwrap_or_else(|| v.to_string())
            }
            _ => v.to_string(),
        }
    }

    fn set_option(&mut self, id: &str, value: String) {
        let Some(sid) = self.session.clone() else {
            self.notice(NoticeLevel::Warn, "No session yet.");
            return;
        };
        if self.turn.is_some() || self.probe.is_some() {
            self.notice(
                NoticeLevel::Warn,
                "Options can only be changed between turns.",
            );
            return;
        }
        self.request(
            "session/set_config_option",
            json!({"sessionId": sid, "configId": id, "value": value}),
            Pending::SetConfig,
        );
    }

    fn pump(&mut self) {
        if !self.idle() {
            return;
        }
        if let Some(target) = self.switch_after_probe.take() {
            match target {
                Some(id) => self.open_load(id, false),
                None => self.open_new(false),
            }
            return;
        }
        if let Some(text) = self.queue.pop_front() {
            self.start_prompt(text);
        }
    }

    /// The command a prompt runs, when wizard answers it rather than the model. A listed
    /// command that is not the user's own (those, and skills, are expanded and go to the
    /// model). With unknown names answered by wizard, any other `/name` that is not a path too.
    fn command_of(&self, text: &str) -> Option<String> {
        let word = text
            .trim_start()
            .strip_prefix('/')?
            .split_whitespace()
            .next()?;
        if self.custom.iter().any(|c| c == word) {
            return None;
        }
        if let Some(c) = self
            .commands
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(word))
        {
            return Some(c.name.to_lowercase());
        }
        let plain = word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':'));
        let path = std::path::Path::new("/").join(word).exists();
        (self.answer_unknown && plain && !path).then(|| word.to_lowercase())
    }

    fn start_prompt(&mut self, text: String) {
        let Some(sid) = self.session.clone() else {
            return;
        };
        self.turn = Some(Turn {
            text: text.clone(),
            buf: String::new(),
        });
        self.request(
            "session/prompt",
            json!({"sessionId": sid, "prompt": [{"type": "text", "text": text}]}),
            Pending::Prompt,
        );
        self.emit(Event::TurnStart);
    }

    // ---- lines from the child ------------------------------------------------

    pub fn on_line(&mut self, line: &str) -> Vec<Action> {
        if let Ok(msg) = serde_json::from_str::<Value>(line) {
            let method = msg.get("method").and_then(Value::as_str);
            let id = msg.get("id");
            match (method, id) {
                (Some(m), Some(id)) => self.on_server_request(m, id.clone(), &msg),
                (Some(m), None) => self.on_notification(m, &msg),
                (None, Some(id)) => {
                    if let Some(id) = id.as_u64() {
                        self.on_response(id, &msg);
                    }
                }
                _ => {}
            }
        }
        self.take()
    }

    fn answer_permission(&mut self, rpc: Value, option: Option<String>) {
        let outcome = match option {
            Some(o) => json!({"outcome": "selected", "optionId": o}),
            None => json!({"outcome": "cancelled"}),
        };
        self.out.push(Action::Send(
            json!({"jsonrpc": "2.0", "id": rpc, "result": {"outcome": outcome}}),
        ));
    }

    /// The agent calling us. Only `session/request_permission` is expected (a finished plan);
    /// everything else is answered "not found" so a turn can never wedge on us.
    fn on_server_request(&mut self, method: &str, id: Value, msg: &Value) {
        if method == "session/request_permission" && self.ask {
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            let call = params.get("toolCall").cloned().unwrap_or(Value::Null);
            let options = params
                .get("options")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let key = match &id {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let tool = self
                .tools
                .get(str_of(&call, "toolCallId"))
                .map(|t| t.name.clone())
                .unwrap_or_default();
            self.asks.insert(key.clone(), (id, options));
            self.emit(Event::Permission(PermissionRequest {
                id: key,
                tool,
                kind: wire::tool_kind(str_of(&call, "kind")),
                title: str_of(&call, "title").to_string(),
                input: call.get("rawInput").cloned().unwrap_or(Value::Null),
                diff: None,
                rule: String::new(),
            }));
            return;
        }
        let reply = if method == "session/request_permission" {
            // Prefer an allow option; wizard says it never asks.
            let opt = msg
                .pointer("/params/options")
                .and_then(Value::as_array)
                .and_then(|a| {
                    a.iter()
                        .find(|o| str_of(o, "kind").starts_with("allow"))
                        .or_else(|| a.first())
                })
                .map(|o| str_of(o, "optionId").to_string());
            match opt {
                Some(o) => {
                    json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": {"outcome": "selected", "optionId": o}}})
                }
                None => {
                    json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": {"outcome": "cancelled"}}})
                }
            }
        } else {
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "Method not found"}})
        };
        self.out.push(Action::Send(reply));
    }

    fn on_notification(&mut self, method: &str, msg: &Value) {
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        match method {
            "session/update" => self.on_update(&params),
            "_wizard/auth_url" => {
                let url = str_of(&params, "url");
                self.notice(NoticeLevel::Warn, format!("Sign in to continue: {url}"));
            }
            _ => {}
        }
    }

    fn on_response(&mut self, id: u64, msg: &Value) {
        let Some(p) = self.pending.remove(&id) else {
            return;
        };
        let err = msg.get("error");
        let result = msg.get("result").cloned().unwrap_or(Value::Null);
        match p {
            Pending::Initialize => {
                if let Some(e) = err {
                    return self.fatal(format!("initialize failed: {}", wire::rpc_error_text(e)));
                }
                if self.backend.is_empty() {
                    let v = result
                        .pointer("/agentInfo/version")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    self.backend = if v.is_empty() {
                        "wizard".into()
                    } else {
                        format!("wizard {v}")
                    };
                }
                match self.resume.take() {
                    Some(id) => self.open_load(id, true),
                    None => self.open_new(true),
                }
            }
            Pending::NewSession { startup } => {
                if let Some(e) = err {
                    let text = wire::rpc_error_text(e);
                    if startup {
                        // no session means no config; say at least which wizard this is
                        let c = self.config();
                        self.emit(Event::ConfigChanged(c));
                        return self.fatal(format!("could not start a session: {text}"));
                    }
                    return self.notice(NoticeLevel::Error, format!("New session failed: {text}"));
                }
                self.session = Some(str_of(&result, "sessionId").to_string());
                self.enter_session(&result);
                let (session_id, config) =
                    (self.session.clone().unwrap_or_default(), self.config());
                self.emit(Event::Ready { session_id, config });
                self.pump();
            }
            Pending::LoadSession { id, startup } => {
                let replay = self.replay.take();
                if let Some(e) = err {
                    let text = wire::rpc_error_text(e);
                    if startup {
                        self.notice(
                            NoticeLevel::Warn,
                            format!("Could not resume {id}: {text}. Starting a new session."),
                        );
                        return self.open_new(true);
                    }
                    return self.notice(
                        NoticeLevel::Error,
                        format!("Could not load session {id}: {text}"),
                    );
                }
                self.session = Some(id.clone());
                self.enter_session(&result);
                let mut replay = replay.unwrap_or_default();
                replay.flush();
                self.emit(Event::History {
                    session_id: id.clone(),
                    items: std::mem::take(&mut replay.items),
                });
                if let Some(t) = replay.todos.take() {
                    self.emit(Event::Todos(t));
                }
                let config = self.config();
                self.emit(Event::Ready {
                    session_id: id,
                    config,
                });
                // wizard keeps no usage for a loaded session, but `/status` answers with the
                // size of the loaded context; without it the sidebar says 0 tokens
                self.probe_after_load = true;
                if !self.start_probe() {
                    self.pump();
                }
            }
            Pending::List { mut acc, pages } => {
                if let Some(e) = err {
                    return self.notice(
                        NoticeLevel::Error,
                        format!("Listing sessions failed: {}", wire::rpc_error_text(e)),
                    );
                }
                for s in result
                    .get("sessions")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let info = wire::parse_session(s);
                    self.session_cwds.insert(info.id.clone(), info.cwd.clone());
                    acc.push(info);
                }
                match result.get("nextCursor").and_then(Value::as_str) {
                    Some(c) if pages < MAX_LIST_PAGES => {
                        let cwd = self.cwd.clone();
                        self.request(
                            "session/list",
                            json!({"cwd": cwd, "cursor": c}),
                            Pending::List {
                                acc,
                                pages: pages + 1,
                            },
                        );
                    }
                    cut => {
                        if cut.is_some() {
                            self.notice(
                                NoticeLevel::Warn,
                                format!(
                                    "Listing stopped after {} sessions; older ones are not shown.",
                                    acc.len()
                                ),
                            );
                        }
                        self.emit(Event::Sessions(acc));
                    }
                }
            }
            Pending::SetConfig => {
                if let Some(e) = err {
                    if wire::is_turn_running_error(e) {
                        return self.notice(
                            NoticeLevel::Warn,
                            "Options can only be changed between turns.",
                        );
                    }
                    return self.notice(
                        NoticeLevel::Error,
                        format!("Could not change the setting: {}", wire::rpc_error_text(e)),
                    );
                }
                if let Some(o) = result.get("configOptions").and_then(Value::as_array) {
                    self.options = o.clone();
                }
                let c = self.config();
                self.emit(Event::ConfigChanged(c));
            }
            Pending::Prompt => self.end_turn(err, &result),
            Pending::Probe => {
                let text = self.probe.take().unwrap_or_default();
                if err.is_none() {
                    if let Some(u) = wire::parse_usage(&text) {
                        self.emit(Event::Usage(u));
                    }
                }
                self.pump();
            }
        }
    }

    fn enter_session(&mut self, result: &Value) {
        if let Some(o) = result.get("configOptions").and_then(Value::as_array) {
            self.options = o.clone();
        }
        self.tools.clear();
        self.turn = None;
        self.probe = None;
    }

    fn end_turn(&mut self, err: Option<&Value>, result: &Value) {
        let command = self.turn.as_ref().and_then(|t| self.command_of(&t.text));
        let Some(turn) = self.turn.take() else { return };
        let slash = command.is_some();
        let text = turn.buf.trim_end().to_string();
        if slash && !text.is_empty() {
            self.notice(NoticeLevel::Info, text.clone());
        }
        let reason = match err {
            Some(e) => {
                self.notice(NoticeLevel::Error, wire::rpc_error_text(e));
                StopReason::Error
            }
            None => wire::stop_reason(str_of(result, "stopReason")),
        };
        self.emit(Event::TurnEnd(reason));
        if matches!(command.as_deref(), Some("status" | "cost")) {
            if let Some(u) = wire::parse_usage(&text) {
                self.emit(Event::Usage(u));
            }
        }
        let wants_probe = !slash || command.as_deref() == Some("compact");
        if wants_probe && err.is_none() && self.start_probe() {
            return;
        }
        self.pump();
    }

    /// Run the hidden `/status` to read the usage lines. False when it cannot run now.
    fn start_probe(&mut self) -> bool {
        if !self.commands.iter().any(|c| c.name == "status") {
            return false;
        }
        let Some(sid) = self.session.clone() else {
            return false;
        };
        self.probe_after_load = false;
        self.probe = Some(String::new());
        self.request(
            "session/prompt",
            json!({"sessionId": sid, "prompt": [{"type": "text", "text": "/status"}]}),
            Pending::Probe,
        );
        true
    }

    // ---- session/update ------------------------------------------------------

    fn on_update(&mut self, params: &Value) {
        let sid = str_of(params, "sessionId");
        let update = params.get("update").unwrap_or(&Value::Null);
        let is_replay = params
            .pointer("/_meta/isReplay")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || update
                .pointer("/_meta/isReplay")
                .and_then(Value::as_bool)
                .unwrap_or(false);
        let kind = str_of(update, "sessionUpdate");

        if is_replay {
            if self.replay.as_ref().is_some_and(|r| r.id == sid) {
                self.on_replay(kind, update);
            }
            return;
        }
        if let Some(active) = &self.session {
            if active != sid {
                return;
            }
        }
        let chunk_text = || {
            update
                .pointer("/content/text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        match kind {
            "agent_message_chunk" => self.on_text(chunk_text()),
            "agent_thought_chunk" if self.probe.is_none() => {
                self.emit(Event::ThoughtDelta(chunk_text()));
            }
            "tool_call" => {
                let call = wire::tool_from_update(update);
                self.tools.insert(call.id.clone(), call.clone());
                self.emit(Event::Tool(call));
            }
            "tool_call_update" => {
                let id = str_of(update, "toolCallId").to_string();
                let call = self.tools.entry(id.clone()).or_insert_with(|| ToolCall {
                    id,
                    ..Default::default()
                });
                wire::apply_tool_update(call, update);
                let call = call.clone();
                let todos = wire::todos_of(&call);
                self.emit(Event::Tool(call));
                if let Some(t) = todos {
                    self.emit(Event::Todos(t));
                }
            }
            "available_commands_update" => {
                self.commands = wire::parse_commands(update);
                self.custom = wire::custom_commands(update);
                let c = self.commands.clone();
                self.emit(Event::Commands(c));
                // a session opened at startup hears about the commands after it loads
                if self.probe_after_load && self.idle() {
                    self.start_probe();
                }
            }
            "config_option_update" => {
                if let Some(o) = update.get("configOptions").and_then(Value::as_array) {
                    self.options = o.clone();
                }
                let c = self.config();
                self.emit(Event::ConfigChanged(c));
            }
            "usage_update" => {
                // Not sent by wizard 3.7.1; the ACP draft shape is {used, size}.
                let used = update.get("used").and_then(Value::as_u64).unwrap_or(0);
                let size = update.get("size").and_then(Value::as_u64).unwrap_or(0);
                if used > 0 || size > 0 {
                    self.emit(Event::Usage(agent_core::Usage {
                        context_tokens: used,
                        context_window: size,
                        ..Default::default()
                    }));
                }
            }
            _ => {}
        }
    }

    fn on_text(&mut self, text: String) {
        if let Some((level, t)) = wire::wizard_notice(&text) {
            return self.notice(level, t);
        }
        if let Some(buf) = &mut self.probe {
            buf.push_str(&text);
        } else if self
            .turn
            .as_ref()
            .is_some_and(|t| self.command_of(&t.text).is_some())
        {
            if let Some(turn) = self.turn.as_mut() {
                turn.buf.push_str(&text);
            }
        } else {
            self.emit(Event::TextDelta(text));
        }
    }

    fn on_replay(&mut self, kind: &str, u: &Value) {
        let Some(r) = self.replay.as_mut() else {
            return;
        };
        let text = u
            .pointer("/content/text")
            .and_then(Value::as_str)
            .unwrap_or("");
        match kind {
            "user_message_chunk" => r.text(ReplayKind::User, text),
            "agent_message_chunk" => r.text(ReplayKind::Assistant, text),
            "agent_thought_chunk" => r.text(ReplayKind::Thought, text),
            "tool_call" => {
                r.flush();
                let call = wire::tool_from_update(u);
                r.tool_idx.insert(call.id.clone(), r.items.len());
                r.items.push(HistoryItem::Tool(call));
            }
            "tool_call_update" => {
                let id = str_of(u, "toolCallId");
                if let Some(&i) = r.tool_idx.get(id) {
                    if let HistoryItem::Tool(call) = &mut r.items[i] {
                        wire::apply_tool_update(call, u);
                        if let Some(t) = wire::todos_of(call) {
                            r.todos = Some(t);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // ---- process end -----------------------------------------------------------

    /// The child's stdout closed without us asking.
    pub fn on_exit(&mut self, detail: &str) -> Vec<Action> {
        self.fatal(detail.to_string());
        self.take()
    }
}
