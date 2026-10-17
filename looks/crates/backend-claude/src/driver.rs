//! The actor behind a [`agent_core::BackendHandle`]: owns the process, turns requests
//! into stream-json and control requests, and feeds stdout through the [`Mapper`].

use crate::history;
use crate::mapper::Mapper;
use crate::proc::{Launch, Proc, SessionArg};
use agent_core::{
    Config, DecideScope, Event, HistoryItem, ImageData, ModelOption, NoticeLevel,
    PermissionRequest, Request, SlashCommand, StopReason,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::time::Instant;

const CALL_TIMEOUT: Duration = Duration::from_secs(20);

pub const MODES: &[&str] = &["default", "acceptEdits", "plan", "bypassPermissions"];

#[derive(Clone, Debug)]
pub struct Opts {
    pub bin: std::ffi::OsString,
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub mode: String,
    pub extra: Vec<String>,
    pub background: bool,
    /// How long an `interrupt` gets before the process is restarted instead.
    pub cancel_grace: Duration,
}

#[derive(Clone, Debug)]
struct ModelInfo {
    value: String,
    resolved: String,
    name: String,
    efforts: Vec<String>,
}

pub struct Driver {
    opts: Opts,
    ev: UnboundedSender<Event>,
    proc: Option<Proc>,
    mapper: Mapper,
    session_id: String,
    model: String,
    effort: String,
    mode: String,
    models: Vec<ModelInfo>,
    commands: Vec<SlashCommand>,
    version: String,
    seq: u64,
    deadline: Option<Instant>,
    /// `can_use_tool` requests the CLI is blocked on, by control request id.
    perms: HashMap<String, PendingPerm>,
    rewind: RewindState,
}

/// What rewind needs to know about the session: the user messages in the order the frontend
/// shows them, and where the previous turn ended.
#[derive(Default)]
struct RewindState {
    turns: Vec<history::TurnRef>,
    last_assistant: Option<String>,
    /// A forked session has no transcript on disk until its first message, so a restart in
    /// that window has to fork again instead of resuming a file that is not there.
    fork: Option<(String, String)>,
}

/// What is needed to answer a `can_use_tool` request later.
struct PendingPerm {
    input: Value,
    /// The CLI's first `permission_suggestions` entry, replayed on "allow always".
    suggestion: Option<Value>,
}

pub fn fresh_uuid() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut b = [0u8; 16];
    for chunk in b.chunks_mut(8) {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        );
        h.write_u32(std::process::id());
        chunk.copy_from_slice(&h.finish().to_le_bytes());
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

pub async fn run(
    opts: Opts,
    first: Proc,
    session: SessionArg,
    replay: bool,
    rx: UnboundedReceiver<Request>,
    ev: UnboundedSender<Event>,
) {
    let mut d = Driver {
        mapper: Mapper::new(opts.cwd.clone()),
        model: opts.model.clone().unwrap_or_else(|| "default".into()),
        effort: opts.effort.clone().unwrap_or_default(),
        mode: opts.mode.clone(),
        opts,
        ev,
        proc: None,
        session_id: session.id().to_string(),
        models: Vec::new(),
        commands: Vec::new(),
        version: String::new(),
        seq: 0,
        deadline: None,
        perms: HashMap::new(),
        rewind: RewindState::default(),
    };
    d.version = claude_version(&d.opts.bin).await;
    d.begin(session, Some(first), replay, true).await;
    d.main_loop(rx).await;
}

async fn claude_version(bin: &std::ffi::OsStr) -> String {
    let out = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new(bin).arg("--version").output(),
    )
    .await;
    match out {
        Ok(Ok(o)) => String::from_utf8_lossy(&o.stdout)
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

impl Driver {
    fn emit(&self, e: Event) {
        let _ = self.ev.send(e);
    }

    fn notice(&self, level: NoticeLevel, text: impl Into<String>) {
        self.emit(Event::Notice {
            level,
            text: text.into(),
        });
    }

    fn launch(&self, session: SessionArg) -> Launch {
        let supports_effort = !self.efforts_for(&self.model).is_empty() || self.models.is_empty();
        Launch {
            bin: self.opts.bin.clone(),
            cwd: self.opts.cwd.clone(),
            session,
            model: (self.model != "default" || self.opts.model.is_some())
                .then(|| self.model.clone()),
            effort: (!self.effort.is_empty() && supports_effort).then(|| self.effort.clone()),
            mode: self.mode.clone(),
            extra: self.opts.extra.clone(),
            background: self.opts.background,
        }
    }

    fn efforts_for(&self, model: &str) -> Vec<String> {
        let base = model.split('[').next().unwrap_or(model);
        self.models
            .iter()
            .find(|m| m.value == model || m.value == base)
            .map(|m| m.efforts.clone())
            .unwrap_or_default()
    }

    fn config(&self) -> Config {
        let mut models: Vec<ModelOption> = self
            .models
            .iter()
            .map(|m| ModelOption {
                id: m.value.clone(),
                name: m.name.clone(),
                provider: "anthropic".into(),
            })
            .collect();
        if !models.iter().any(|m| m.id == self.model) {
            models.push(ModelOption {
                id: self.model.clone(),
                name: self.model.clone(),
                provider: "anthropic".into(),
            });
        }
        Config {
            model: self.model.clone(),
            models,
            effort: self.effort.clone(),
            efforts: self.efforts_for(&self.model),
            mode: self.mode.clone(),
            modes: MODES.iter().map(|s| s.to_string()).collect(),
            cwd: self.opts.cwd.to_string_lossy().into_owned(),
            backend: if self.version.is_empty() {
                "claude".into()
            } else {
                format!("claude {}", self.version)
            },
        }
    }

    /// (Re)start the process for `session` and run the handshake. `announce` sends
    /// Ready and Commands; `replay` adds the transcript as History.
    async fn begin(
        &mut self,
        session: SessionArg,
        pre: Option<Proc>,
        replay: bool,
        announce: bool,
    ) {
        if let Some(mut old) = self.proc.take() {
            old.kill_now().await;
        }
        self.session_id = session.id().to_string();
        let history = if replay {
            history::load(&self.opts.cwd, &self.session_id)
        } else {
            None
        };
        match &session {
            SessionArg::New(_) => self.rewind = RewindState::default(),
            SessionArg::Resume(_) if replay => {
                let (turns, last) = history::session_path(&self.opts.cwd, &self.session_id)
                    .and_then(|p| std::fs::File::open(p).ok())
                    .map(|f| history::turns(std::io::BufReader::new(f)))
                    .unwrap_or_default();
                self.rewind = RewindState {
                    turns,
                    last_assistant: last,
                    fork: None,
                };
            }
            _ => {}
        }
        let proc = match pre {
            Some(p) => p,
            None => match Proc::start(&self.launch(session)) {
                Ok(p) => p,
                Err(e) => return self.emit(Event::Fatal(format!("{e:#}"))),
            },
        };
        self.proc = Some(proc);
        self.mapper.reset_session();
        self.mapper.set_cost_offset(self.mapper.cost());
        self.mapper.session_id = String::new();
        self.deadline = None;
        self.perms.clear();
        if let Err(e) = self.handshake().await {
            // A process that exited already reported itself through on_exit.
            if self.proc.is_some() {
                self.fail(format!("claude did not answer the handshake: {e}"))
                    .await;
            }
            return;
        }
        if announce {
            self.emit(Event::Ready {
                session_id: self.session_id.clone(),
                config: self.config(),
            });
            self.emit(Event::Commands(self.commands.clone()));
        }
        match (replay, history) {
            (true, Some(items)) => self.emit(Event::History {
                session_id: self.session_id.clone(),
                items,
            }),
            (true, None) => self.emit(Event::History {
                session_id: self.session_id.clone(),
                items: Vec::<HistoryItem>::new(),
            }),
            _ => {}
        }
    }

    async fn handshake(&mut self) -> Result<(), String> {
        let init = self.call("initialize", json!({})).await?;
        self.models = parse_models(&init);
        self.commands = parse_commands(&init);
        if let Some(m) = init.get("current_permission_mode").and_then(Value::as_str) {
            self.mode = m.to_string();
        }
        let settings = self.call("get_settings", json!({})).await.ok();
        let eff = settings.as_ref().and_then(|s| s.get("effective"));
        let requested = self.opts.model.clone().or_else(|| {
            eff.and_then(|e| e.get("model"))
                .and_then(Value::as_str)
                .map(str::to_string)
        });
        if self.effort.is_empty() {
            self.effort = eff
                .and_then(|e| e.get("effortLevel"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
        }
        // Keep the model the user picked in this process if there is one; otherwise ask the CLI.
        let want = if self.model != "default" {
            Some(self.model.clone())
        } else {
            requested
        };
        let ctx = self
            .call("get_context_usage", json!({"detail": "summary"}))
            .await
            .ok();
        let actual = ctx
            .as_ref()
            .and_then(|c| c.get("model"))
            .and_then(Value::as_str)
            .map(str::to_string);
        self.model = match (want, actual) {
            (Some(w), Some(a)) => {
                if self.models.iter().any(|m| m.value == w && m.resolved == a) || w.contains('[') {
                    w
                } else {
                    self.models
                        .iter()
                        .find(|m| m.resolved == a)
                        .map(|m| m.value.clone())
                        .unwrap_or(a)
                }
            }
            (Some(w), None) => w,
            (None, Some(a)) => self
                .models
                .iter()
                .find(|m| m.resolved == a)
                .map(|m| m.value.clone())
                .unwrap_or(a),
            (None, None) => "default".to_string(),
        };
        if let Some(c) = &ctx {
            self.mapper
                .set_window(c.get("maxTokens").and_then(Value::as_u64).unwrap_or(0));
        }
        if self.efforts_for(&self.model).is_empty() && !self.models.is_empty() {
            self.effort.clear();
        }
        Ok(())
    }

    /// Send a control request and wait for its response. Other output keeps flowing
    /// to the frontend while we wait.
    async fn call(&mut self, subtype: &str, mut body: Value) -> Result<Value, String> {
        self.seq += 1;
        let id = format!("openc-{}", self.seq);
        body["subtype"] = json!(subtype);
        let req = json!({"type": "control_request", "request_id": id, "request": body});
        let proc = self.proc.as_mut().ok_or("claude is not running")?;
        if let Err(e) = proc.send(&req).await {
            // A dead process shows up as EPIPE here before stdout reports EOF.
            self.on_exit().await;
            return Err(e.to_string());
        }
        let end = Instant::now() + CALL_TIMEOUT;
        loop {
            let proc = self.proc.as_mut().ok_or("claude exited")?;
            match tokio::time::timeout_at(end, proc.lines.recv()).await {
                Err(_) => return Err(format!("{subtype} timed out")),
                Ok(None) => {
                    self.on_exit().await;
                    return Err("claude exited".into());
                }
                Ok(Some(line)) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&line) {
                        if v.get("type").and_then(Value::as_str) == Some("control_response") {
                            let r = v.get("response").cloned().unwrap_or(Value::Null);
                            if r.get("request_id").and_then(Value::as_str) == Some(id.as_str()) {
                                return if r.get("subtype").and_then(Value::as_str) == Some("error")
                                {
                                    Err(r
                                        .get("error")
                                        .and_then(Value::as_str)
                                        .unwrap_or("request failed")
                                        .to_string())
                                } else {
                                    Ok(r.get("response").cloned().unwrap_or(Value::Null))
                                };
                            }
                            continue;
                        }
                    }
                    self.handle_line(&line).await;
                }
            }
        }
    }

    async fn handle_line(&mut self, line: &str) {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return;
        };
        if v.get("type").and_then(Value::as_str) == Some("control_request") {
            let sub = v
                .pointer("/request/subtype")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string();
            if sub == "can_use_tool" {
                return self.permission_request(&v);
            }
            // An unanswered request would hang the turn.
            let reply = json!({"type": "control_response", "response": {"subtype": "error", "request_id": v.get("request_id"), "error": format!("openc does not handle {sub}")}});
            if let Some(p) = self.proc.as_mut() {
                let _ = p.send(&reply).await;
            }
            return;
        }
        // Leaving plan mode changes the mode in the middle of a turn, and says so here.
        if v.get("type").and_then(Value::as_str) == Some("system")
            && v.get("subtype").and_then(Value::as_str) == Some("status")
        {
            if let Some(m) = v.get("permissionMode").and_then(Value::as_str) {
                if m != self.mode {
                    self.mode = m.to_string();
                    self.emit(Event::ConfigChanged(self.config()));
                }
            }
        }
        if v.get("type").and_then(Value::as_str) == Some("assistant")
            && v.get("parent_tool_use_id").is_none_or(Value::is_null)
        {
            if let Some(u) = v.get("uuid").and_then(Value::as_str) {
                self.rewind.last_assistant = Some(u.to_string());
            }
        }
        let events = self.mapper.feed(&v);
        let is_init = v.get("type").and_then(Value::as_str) == Some("system")
            && v.get("subtype").and_then(Value::as_str) == Some("init");
        for e in events {
            self.emit(e);
        }
        if is_init {
            self.after_init();
        }
        if !self.mapper.in_turn {
            self.deadline = None;
        }
    }

    /// The CLI asks whether a tool may run (`--permission-prompt-tool stdio`). The call stays
    /// blocked until [`Request::Decide`] answers it.
    fn permission_request(&mut self, v: &Value) {
        let Some(id) = v.get("request_id").and_then(Value::as_str) else {
            return;
        };
        let req = v.get("request").cloned().unwrap_or(Value::Null);
        let name = req.get("tool_name").and_then(Value::as_str).unwrap_or("");
        let input = req.get("input").cloned().unwrap_or(Value::Null);
        let suggestion = req
            .get("permission_suggestions")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .cloned();
        let rule = suggestion
            .as_ref()
            .map(suggestion_label)
            .unwrap_or_default();
        let p = PermissionRequest {
            id: id.to_string(),
            tool: name.to_string(),
            kind: crate::tools::kind_of(name),
            title: crate::tools::title_of(name, &input, &self.opts.cwd),
            diff: crate::tools::diff_of(name, &input),
            input: input.clone(),
            rule,
        };
        self.perms
            .insert(id.to_string(), PendingPerm { input, suggestion });
        self.emit(Event::Permission(p));
    }

    async fn decide(&mut self, id: &str, allow: bool, scope: DecideScope, note: &str) {
        let Some(pend) = self.perms.remove(id) else {
            return;
        };
        let body = if allow {
            let mut b = json!({"behavior": "allow", "updatedInput": pend.input});
            if scope == DecideScope::Always {
                if let Some(mut sug) = pend.suggestion {
                    // Keep the grant to this session instead of writing settings files.
                    if sug.get("type").and_then(Value::as_str) == Some("addRules") {
                        sug["destination"] = json!("session");
                    }
                    b["updatedPermissions"] = json!([sug]);
                }
            }
            b
        } else {
            // The frontend recognises this opening to show the call as denied, and the model
            // gets to know it was a person, not a rule.
            let msg = if note.trim().is_empty() {
                "The user denied this tool call.".to_string()
            } else {
                format!("The user denied this tool call and said: {}", note.trim())
            };
            json!({"behavior": "deny", "message": msg})
        };
        let reply = json!({"type": "control_response", "response": {"subtype": "success", "request_id": id, "response": body}});
        if let Some(p) = self.proc.as_mut() {
            let _ = p.send(&reply).await;
        }
    }

    /// Let an `AskUserQuestion` through with the person's answers filled in. The CLI wants
    /// them as `answers: {question: "pick, pick"}` next to the original `questions`.
    async fn answer(&mut self, id: &str, answers: Vec<(String, String)>) {
        let Some(pend) = self.perms.remove(id) else {
            return;
        };
        let mut input = pend.input;
        let map: serde_json::Map<String, Value> = answers
            .into_iter()
            .map(|(q, a)| (q, Value::String(a)))
            .collect();
        input["answers"] = Value::Object(map);
        let body = json!({"behavior": "allow", "updatedInput": input});
        let reply = json!({"type": "control_response", "response": {"subtype": "success", "request_id": id, "response": body}});
        if let Some(p) = self.proc.as_mut() {
            let _ = p.send(&reply).await;
        }
    }

    /// Answer every open permission request with a denial, so an interrupt is not stuck
    /// behind a prompt nobody will answer.
    async fn deny_all(&mut self, why: &str) {
        let ids: Vec<String> = self.perms.keys().cloned().collect();
        for id in ids {
            self.decide(&id, false, DecideScope::Once, why).await;
        }
    }

    /// The CLI can change model or mode on its own (plan mode exit, `/model`).
    fn after_init(&mut self) {
        let mut changed = false;
        if !self.mapper.session_id.is_empty() {
            self.session_id = self.mapper.session_id.clone();
        }
        let resolved = self
            .models
            .iter()
            .find(|m| m.value == self.model)
            .map(|m| m.resolved.clone());
        if !self.mapper.model.is_empty()
            && resolved.as_deref().is_some_and(|r| r != self.mapper.model)
            && !self.model.contains('[')
        {
            if let Some(m) = self.models.iter().find(|m| m.resolved == self.mapper.model) {
                self.model = m.value.clone();
                changed = true;
            }
        }
        if !self.mapper.mode.is_empty() && self.mapper.mode != self.mode {
            self.mode = self.mapper.mode.clone();
            changed = true;
        }
        if changed {
            self.emit(Event::ConfigChanged(self.config()));
        }
    }

    async fn on_exit(&mut self) {
        let Some(mut p) = self.proc.take() else {
            return;
        };
        self.perms.clear();
        let status = p.exit_text().await;
        let tail = p.stderr_tail();
        let why = if tail.is_empty() {
            format!("claude exited ({status})")
        } else {
            format!("claude exited ({status}): {tail}")
        };
        for e in self.mapper.abort_turn(StopReason::Error, "claude exited") {
            self.emit(e);
        }
        self.deadline = None;
        self.emit(Event::Fatal(why));
    }

    async fn fail(&mut self, why: String) {
        let tail = self
            .proc
            .as_ref()
            .map(|p| p.stderr_tail())
            .unwrap_or_default();
        if let Some(mut p) = self.proc.take() {
            p.kill_now().await;
        }
        let why = if tail.is_empty() {
            why
        } else {
            format!("{why}: {tail}")
        };
        self.emit(Event::Fatal(why));
    }

    async fn main_loop(&mut self, mut rx: UnboundedReceiver<Request>) {
        loop {
            let deadline = self.deadline;
            tokio::select! {
                req = rx.recv() => match req {
                    None | Some(Request::Shutdown) => break,
                    Some(r) => self.on_request(r).await,
                },
                line = async {
                    match self.proc.as_mut() {
                        Some(p) => p.lines.recv().await,
                        None => std::future::pending().await,
                    }
                } => match line {
                    Some(l) => self.handle_line(&l).await,
                    None => self.on_exit().await,
                },
                _ = async {
                    match deadline {
                        Some(t) => tokio::time::sleep_until(t).await,
                        None => std::future::pending().await,
                    }
                } => self.cancel_timed_out().await,
            }
        }
        self.shutdown().await;
    }

    async fn shutdown(&mut self) {
        if let Some(mut p) = self.proc.take() {
            if self.mapper.in_turn {
                let _ = p.send(&json!({"type": "control_request", "request_id": "openc-bye", "request": {"subtype": "interrupt"}})).await;
            }
            p.shutdown(Duration::from_secs(3)).await;
        }
    }

    async fn cancel_timed_out(&mut self) {
        self.deadline = None;
        if !self.mapper.in_turn {
            return;
        }
        self.notice(
            NoticeLevel::Warn,
            "claude did not stop after the interrupt; restarting it (the session is kept)",
        );
        if let Some(mut p) = self.proc.take() {
            p.kill_now().await;
        }
        for e in self.mapper.abort_turn(StopReason::Cancelled, "interrupted") {
            self.emit(e);
        }
        let s = self.same_session();
        self.begin(s, None, false, false).await;
    }

    /// The current session as a launch argument: resumable once a transcript exists.
    fn same_session(&self) -> SessionArg {
        if let Some((from, at)) = &self.rewind.fork {
            if history::session_path(&self.opts.cwd, &self.session_id).is_none() {
                return SessionArg::Fork {
                    from: from.clone(),
                    at: at.clone(),
                    new_id: self.session_id.clone(),
                };
            }
        }
        if history::session_path(&self.opts.cwd, &self.session_id).is_some() {
            SessionArg::Resume(self.session_id.clone())
        } else {
            SessionArg::New(self.session_id.clone())
        }
    }

    async fn on_request(&mut self, r: Request) {
        match r {
            Request::Prompt(text) => self.prompt(text).await,
            Request::PromptWith { text, images } => self.prompt_with(text, images).await,
            Request::Decide {
                id,
                allow,
                scope,
                note,
            } => self.decide(&id, allow, scope, &note).await,
            Request::Answer { id, answers } => self.answer(&id, answers).await,
            Request::RewindPreview { turn } => self.rewind_preview(turn).await,
            Request::Rewind {
                turn,
                conversation,
                files,
            } => self.rewind(turn, conversation, files).await,
            Request::Steer(text) => {
                // The CLI folds a message sent mid-turn into the running turn at the next tool
                // boundary, so steering is just a prompt that does not start a new turn.
                if self.proc.is_some() && self.mapper.in_turn {
                    let uuid = fresh_uuid();
                    let msg = json!({"type": "user", "uuid": uuid, "message": {"role": "user", "content": text}});
                    // Counted so later turns keep their index, but no clean cut point.
                    self.rewind.turns.push(history::TurnRef {
                        uuid,
                        text: text.clone(),
                        prev: None,
                    });
                    if let Some(p) = self.proc.as_mut() {
                        let _ = p.send(&msg).await;
                    }
                } else {
                    self.prompt(text).await;
                }
            }
            Request::Cancel => {
                self.deny_all("Interrupted by the user.").await;
                if self.proc.is_some() && self.mapper.in_turn {
                    let req = json!({"type": "control_request", "request_id": format!("openc-int-{}", self.seq), "request": {"subtype": "interrupt"}});
                    self.seq += 1;
                    if let Some(p) = self.proc.as_mut() {
                        let _ = p.send(&req).await;
                    }
                    self.deadline = Some(Instant::now() + self.opts.cancel_grace);
                }
            }
            Request::NewSession => {
                for e in self
                    .mapper
                    .abort_turn(StopReason::Cancelled, "session replaced")
                {
                    self.emit(e);
                }
                let id = fresh_uuid();
                self.begin(SessionArg::New(id.clone()), None, false, true)
                    .await;
                self.emit(Event::History {
                    session_id: id,
                    items: Vec::new(),
                });
            }
            Request::LoadSession(id) => {
                if history::session_path(&self.opts.cwd, &id).is_none() {
                    return self.notice(
                        NoticeLevel::Error,
                        format!("session {id} not found for this directory"),
                    );
                }
                for e in self
                    .mapper
                    .abort_turn(StopReason::Cancelled, "session replaced")
                {
                    self.emit(e);
                }
                self.begin(SessionArg::Resume(id), None, true, true).await;
            }
            Request::ListSessions => {
                let cwd = self.opts.cwd.clone();
                let list = tokio::task::spawn_blocking(move || history::list_sessions(&cwd, 100))
                    .await
                    .unwrap_or_default();
                self.emit(Event::Sessions(list));
            }
            Request::SetModel(m) => self.set_model(m).await,
            Request::SetEffort(e) => self.set_effort(e).await,
            Request::SetMode(m) => self.set_mode(m).await,
            Request::Refresh => self.refresh().await,
            Request::Shutdown => {}
        }
    }

    async fn prompt(&mut self, text: String) {
        let t = text.trim();
        // Commands that change what Config shows go through the control protocol so the
        // frontend hears about them; the CLI would run them as local commands silently.
        // A frontend can still treat every Prompt as TurnStart .. TurnEnd.
        let local = command_arg(t, "model").is_some()
            || command_arg(t, "effort").is_some()
            || ["clear", "new", "reset"]
                .iter()
                .any(|c| command_arg(t, c).is_some());
        if local {
            self.rewind.turns.push(history::TurnRef {
                text: text.clone(),
                ..Default::default()
            });
            let wrap = !self.mapper.in_turn;
            if wrap {
                self.emit(Event::TurnStart);
            }
            self.local_command(t).await;
            if wrap {
                self.emit(Event::TurnEnd(StopReason::EndTurn));
            }
            return;
        }
        if self.proc.is_none() {
            // After a Fatal: bring the process back on the same session.
            let s = self.same_session();
            self.begin(s, None, false, true).await;
            if self.proc.is_none() {
                return;
            }
        }
        self.send_user(json!(text.clone()), text).await;
    }

    /// A prompt with images: one user message whose content is image blocks, then the text.
    async fn prompt_with(&mut self, text: String, images: Vec<ImageData>) {
        if images.is_empty() {
            return self.prompt(text).await;
        }
        if self.proc.is_none() {
            let s = self.same_session();
            self.begin(s, None, false, true).await;
            if self.proc.is_none() {
                return;
            }
        }
        self.send_user(user_content(&text, &images), text).await;
    }

    async fn send_user(&mut self, content: Value, text: String) {
        let uuid = fresh_uuid();
        let msg =
            json!({"type": "user", "uuid": uuid, "message": {"role": "user", "content": content}});
        let prev = self.rewind.last_assistant.clone();
        let sent = match self.proc.as_mut() {
            Some(p) => p.send(&msg).await,
            None => return,
        };
        if sent.is_err() {
            return self.on_exit().await;
        }
        self.rewind
            .turns
            .push(history::TurnRef { uuid, text, prev });
        if let Some(e) = self.mapper.begin_turn() {
            self.emit(e);
        }
    }

    /// `rewind_files` for the message at `turn`. `dry_run` only reports what would change.
    async fn rewind_files_call(&mut self, uuid: &str, dry_run: bool) -> Result<Value, String> {
        if uuid.is_empty() {
            return Err("this message has no file snapshot".into());
        }
        let mut body = json!({"user_message_id": uuid});
        if dry_run {
            body["dry_run"] = json!(true);
        }
        let r = self.call("rewind_files", body).await?;
        if r.get("canRewind").and_then(Value::as_bool) == Some(false) {
            return Err(r
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("no snapshot for this message")
                .to_string());
        }
        Ok(r)
    }

    async fn rewind_preview(&mut self, turn: usize) {
        let mut p = agent_core::RewindPreview {
            turn,
            ..Default::default()
        };
        let Some(t) = self.rewind.turns.get(turn).cloned() else {
            p.note = "this message is not in the session record".into();
            return self.emit(Event::RewindPreview(p));
        };
        if self.mapper.in_turn {
            p.note = "finish or interrupt the running turn first".into();
            return self.emit(Event::RewindPreview(p));
        }
        p.can_rewind_conversation = !t.uuid.is_empty() && (t.prev.is_some() || turn == 0);
        match self.rewind_files_call(&t.uuid, true).await {
            Ok(r) => {
                p.can_rewind_files = true;
                p.files = r
                    .get("filesChanged")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                let n = |k: &str| r.get(k).and_then(Value::as_u64).unwrap_or(0) as usize;
                p.insertions = n("insertions");
                p.deletions = n("deletions");
            }
            Err(e) => p.note = format!("files: {e}"),
        }
        if !p.can_rewind_conversation && p.note.is_empty() {
            p.note = "the conversation cannot be cut at this message".into();
        }
        self.emit(Event::RewindPreview(p));
    }

    async fn rewind(&mut self, turn: usize, conversation: bool, files: bool) {
        let Some(t) = self.rewind.turns.get(turn).cloned() else {
            return self.notice(
                NoticeLevel::Error,
                "rewind: that message is not in the session record",
            );
        };
        if self.mapper.in_turn {
            return self.notice(
                NoticeLevel::Warn,
                "rewind: finish or interrupt the running turn first",
            );
        }
        if files {
            match self.rewind_files_call(&t.uuid, false).await {
                Ok(_) => self.notice(NoticeLevel::Info, "files put back"),
                Err(e) => return self.notice(NoticeLevel::Error, format!("rewind failed: {e}")),
            }
        }
        if !conversation {
            return;
        }
        let items =
            history::load_before(&self.opts.cwd, &self.session_id, &t.uuid).unwrap_or_default();
        let new_id = fresh_uuid();
        let (arg, fork) = match &t.prev {
            Some(at) if !t.uuid.is_empty() => (
                SessionArg::Fork {
                    from: self.session_id.clone(),
                    at: at.clone(),
                    new_id: new_id.clone(),
                },
                Some((self.session_id.clone(), at.clone())),
            ),
            _ if turn == 0 => (SessionArg::New(new_id.clone()), None),
            _ => {
                return self.notice(
                    NoticeLevel::Error,
                    "rewind: the conversation cannot be cut at this message",
                );
            }
        };
        let kept: Vec<history::TurnRef> = self.rewind.turns[..turn].to_vec();
        let last = t.prev.clone();
        self.begin(arg, None, false, false).await;
        if self.proc.is_none() {
            return;
        }
        self.rewind = RewindState {
            turns: kept,
            last_assistant: last,
            fork,
        };
        self.emit(Event::History {
            session_id: new_id,
            items,
        });
    }

    async fn local_command(&mut self, t: &str) {
        if let Some(arg) = command_arg(t, "model") {
            if arg.is_empty() {
                let list = self
                    .models
                    .iter()
                    .map(|m| m.value.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                self.notice(
                    NoticeLevel::Info,
                    format!("Current model: {}. Available: {list}", self.model),
                );
            } else {
                self.set_model(arg.to_string()).await;
            }
        } else if let Some(arg) = command_arg(t, "effort") {
            if arg.is_empty() {
                self.notice(
                    NoticeLevel::Info,
                    format!(
                        "Effort: {}",
                        if self.effort.is_empty() {
                            "default"
                        } else {
                            &self.effort
                        }
                    ),
                );
            } else {
                self.set_effort(arg.to_string()).await;
            }
        } else {
            Box::pin(self.on_request(Request::NewSession)).await;
        }
    }

    async fn set_model(&mut self, m: String) {
        match self.call("set_model", json!({"model": m})).await {
            Ok(_) => {
                self.model = m;
                if self.efforts_for(&self.model).is_empty() && !self.models.is_empty() {
                    self.effort.clear();
                }
                self.emit(Event::ConfigChanged(self.config()));
                if let Ok(c) = self
                    .call("get_context_usage", json!({"detail": "summary"}))
                    .await
                {
                    let (total, max) = (
                        c.get("totalTokens").and_then(Value::as_u64).unwrap_or(0),
                        c.get("maxTokens").and_then(Value::as_u64).unwrap_or(0),
                    );
                    let u = self.mapper.usage_with(total, max);
                    self.emit(Event::Usage(u));
                }
            }
            Err(e) => self.notice(NoticeLevel::Error, format!("could not switch model: {e}")),
        }
    }

    async fn set_effort(&mut self, e: String) {
        let allowed = self.efforts_for(&self.model);
        if allowed.is_empty() && !self.models.is_empty() {
            return self.notice(
                NoticeLevel::Error,
                format!("{} has no effort setting", self.model),
            );
        }
        if e != "auto" && !allowed.is_empty() && !allowed.contains(&e) {
            return self.notice(
                NoticeLevel::Error,
                format!("effort must be one of {}", allowed.join(", ")),
            );
        }
        let value = if e == "auto" { Value::Null } else { json!(e) };
        match self
            .call(
                "apply_flag_settings",
                json!({"settings": {"effortLevel": value}}),
            )
            .await
        {
            Ok(_) => {
                self.effort = if e == "auto" { String::new() } else { e };
                self.emit(Event::ConfigChanged(self.config()));
            }
            Err(err) => self.notice(NoticeLevel::Error, format!("could not set effort: {err}")),
        }
    }

    async fn set_mode(&mut self, m: String) {
        match self.call("set_permission_mode", json!({"mode": m})).await {
            Ok(r) => {
                self.mode = r
                    .get("mode")
                    .and_then(Value::as_str)
                    .unwrap_or(&m)
                    .to_string();
                self.emit(Event::ConfigChanged(self.config()));
            }
            Err(e) => self.notice(NoticeLevel::Error, format!("could not change mode: {e}")),
        }
    }

    async fn refresh(&mut self) {
        match self.call("initialize", json!({})).await {
            Ok(init) => {
                self.models = parse_models(&init);
                self.commands = parse_commands(&init);
                self.emit(Event::Commands(self.commands.clone()));
                self.emit(Event::ConfigChanged(self.config()));
            }
            Err(e) => self.notice(NoticeLevel::Error, format!("refresh failed: {e}")),
        }
    }
}

/// Short text for the "allow always" choice of a permission suggestion.
fn suggestion_label(s: &Value) -> String {
    match s.get("type").and_then(Value::as_str) {
        Some("addRules") => s
            .pointer("/rules/0/ruleContent")
            .and_then(Value::as_str)
            .or_else(|| s.pointer("/rules/0/toolName").and_then(Value::as_str))
            .unwrap_or("")
            .to_string(),
        Some("setMode") => match s.get("mode").and_then(Value::as_str) {
            Some("acceptEdits") => "edits this session".to_string(),
            Some(m) => format!("{m} this session"),
            None => String::new(),
        },
        _ => String::new(),
    }
}

/// `/name` or `/name args` -> the args, trimmed.
/// The `content` array of a user message carrying images: each image block, then the text.
pub fn user_content(text: &str, images: &[ImageData]) -> Value {
    let mut blocks: Vec<Value> = images
        .iter()
        .map(|i| {
            json!({"type": "image", "source": {"type": "base64", "media_type": i.media_type, "data": i.base64}})
        })
        .collect();
    if !text.trim().is_empty() {
        blocks.push(json!({"type": "text", "text": text}));
    }
    Value::Array(blocks)
}

fn command_arg<'a>(t: &'a str, name: &str) -> Option<&'a str> {
    let rest = t.strip_prefix('/')?.strip_prefix(name)?;
    if rest.is_empty() || rest.starts_with(char::is_whitespace) {
        Some(rest.trim())
    } else {
        None
    }
}

fn parse_models(init: &Value) -> Vec<ModelInfo> {
    let mut out: Vec<ModelInfo> = Vec::new();
    let Some(list) = init.get("models").and_then(Value::as_array) else {
        return fallback_models();
    };
    for m in list {
        let s = |k: &str| m.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let efforts = m
            .get("supportedEffortLevels")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        out.push(ModelInfo {
            value: s("value"),
            resolved: s("resolvedModel"),
            name: s("displayName"),
            efforts,
        });
    }
    // The CLI accepts a `[1m]` suffix on the three aliases (its /model help lists them).
    let ones: Vec<ModelInfo> = out
        .iter()
        .filter(|m| matches!(m.value.as_str(), "opus" | "sonnet" | "fable"))
        .map(|m| ModelInfo {
            value: format!("{}[1m]", m.value),
            resolved: m.resolved.clone(),
            name: format!("{} (1M)", m.name),
            efforts: m.efforts.clone(),
        })
        .collect();
    out.extend(ones);
    out
}

fn fallback_models() -> Vec<ModelInfo> {
    let lv = ["low", "medium", "high"].map(String::from).to_vec();
    vec![
        ModelInfo {
            value: "opus".into(),
            resolved: String::new(),
            name: "Opus".into(),
            efforts: lv.clone(),
        },
        ModelInfo {
            value: "sonnet".into(),
            resolved: String::new(),
            name: "Sonnet".into(),
            efforts: lv,
        },
        ModelInfo {
            value: "haiku".into(),
            resolved: String::new(),
            name: "Haiku".into(),
            efforts: vec![],
        },
    ]
}

fn parse_commands(init: &Value) -> Vec<SlashCommand> {
    let Some(list) = init.get("commands").and_then(Value::as_array) else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|c| {
            let name = c.get("name")?.as_str()?.to_string();
            let desc = c.get("description").and_then(Value::as_str).unwrap_or("");
            // The CLI tags non-built-ins with their source: "... (user)".
            let desc = desc.strip_suffix(" (user)").unwrap_or(desc).to_string();
            let hint = c
                .get("argumentHint")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Some(SlashCommand {
                name,
                description: desc,
                input_hint: hint,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_arg_needs_a_word_boundary() {
        assert_eq!(command_arg("/model opus", "model"), Some("opus"));
        assert_eq!(command_arg("/model", "model"), Some(""));
        assert_eq!(command_arg("/modelx", "model"), None);
        assert_eq!(command_arg("model opus", "model"), None);
    }

    fn control(id: &str) -> Value {
        include_str!("../tests/fixtures/control.jsonl")
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .find(|v| v.pointer("/response/request_id").and_then(Value::as_str) == Some(id))
            .map(|v| v["response"]["response"].clone())
            .unwrap()
    }

    #[test]
    fn models_come_from_initialize_with_1m_variants() {
        let models = parse_models(&control("initialize"));
        let opus = models.iter().find(|m| m.value == "opus").unwrap();
        assert_eq!(opus.resolved, "claude-opus-5-5");
        assert_eq!(opus.efforts, ["low", "medium", "high", "xhigh", "max"]);
        let haiku = models.iter().find(|m| m.value == "haiku").unwrap();
        assert!(haiku.efforts.is_empty(), "haiku has no effort setting");
        assert!(models
            .iter()
            .any(|m| m.value == "opus[1m]" && m.name.ends_with("(1M)")));
        assert!(models.iter().any(|m| m.value == "claude-opus-4-8"));
        assert!(!models.iter().any(|m| m.value == "haiku[1m]"));
    }

    #[test]
    fn commands_come_from_initialize_and_drop_the_source_tag() {
        let cmds = parse_commands(&control("initialize"));
        let c = cmds.iter().find(|c| c.name == "gauntlet-loop").unwrap();
        assert_eq!(c.input_hint, "<goal> [against <bar>]");
        assert!(!c.description.ends_with("(user)"));
    }

    #[test]
    fn efforts_follow_the_model_even_with_a_1m_suffix() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut d = Driver {
            opts: Opts {
                bin: "claude".into(),
                cwd: "/w".into(),
                model: None,
                effort: None,
                mode: "default".into(),
                extra: vec![],
                background: false,
                cancel_grace: Duration::from_secs(6),
            },
            ev: tx,
            proc: None,
            mapper: Mapper::new("/w".into()),
            session_id: String::new(),
            model: "opus[1m]".into(),
            effort: "high".into(),
            mode: "default".into(),
            models: parse_models(&control("initialize")),
            commands: vec![],
            version: "2.1.289".into(),
            seq: 0,
            deadline: None,
            perms: HashMap::new(),
            rewind: RewindState::default(),
        };
        assert_eq!(d.config().efforts.len(), 5);
        assert_eq!(d.config().backend, "claude 2.1.289");
        assert_eq!(
            d.config().modes,
            ["default", "acceptEdits", "plan", "bypassPermissions"]
        );
        d.model = "haiku".into();
        assert!(d.config().efforts.is_empty());
        d.model = "claude-something-new".into();
        assert!(
            d.config()
                .models
                .iter()
                .any(|m| m.id == "claude-something-new"),
            "an unlisted current model still appears in the list"
        );
    }

    #[tokio::test]
    async fn a_status_line_after_a_plan_is_approved_moves_the_mode() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut d = Driver {
            opts: Opts {
                bin: "claude".into(),
                cwd: "/w".into(),
                model: None,
                effort: None,
                mode: "plan".into(),
                extra: vec![],
                background: false,
                cancel_grace: Duration::from_secs(6),
            },
            ev: tx,
            proc: None,
            mapper: Mapper::new("/w".into()),
            session_id: String::new(),
            model: "haiku".into(),
            effort: String::new(),
            mode: "plan".into(),
            models: vec![],
            commands: vec![],
            version: String::new(),
            seq: 0,
            deadline: None,
            perms: HashMap::new(),
            rewind: RewindState::default(),
        };
        d.handle_line(
            r#"{"type":"system","subtype":"status","status":null,"permissionMode":"acceptEdits"}"#,
        )
        .await;
        let Some(Event::ConfigChanged(c)) = rx.recv().await else {
            panic!("no config change");
        };
        assert_eq!(c.mode, "acceptEdits");
        // The same mode again says nothing.
        d.handle_line(r#"{"type":"system","subtype":"status","permissionMode":"acceptEdits"}"#)
            .await;
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn uuid_shape() {
        let u = fresh_uuid();
        assert_eq!(u.len(), 36);
        assert_eq!(u.as_bytes()[14], b'4');
        assert_ne!(u, fresh_uuid());
    }

    #[test]
    fn images_go_first_then_the_text_as_blocks() {
        let img = ImageData {
            media_type: "image/png".into(),
            base64: "AAAA".into(),
        };
        let c = user_content("what is this", &[img.clone(), img]);
        let a = c.as_array().unwrap();
        assert_eq!(a.len(), 3);
        assert_eq!(a[0]["type"], "image");
        assert_eq!(a[0]["source"]["media_type"], "image/png");
        assert_eq!(a[0]["source"]["data"], "AAAA");
        assert_eq!(a[2], json!({"type": "text", "text": "what is this"}));
        // No text: the image alone.
        assert_eq!(user_content(" ", &[]).as_array().unwrap().len(), 0);
    }
}
