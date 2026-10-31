//! The hook engine: matches hooks to an event, runs them, and folds what
//! they said into one [`Outcome`].

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use super::event::HookEvent;
use super::matcher::Matcher;
use super::protocol::{self, PermissionRequestDecision, Verdict, Verdicts};
use super::spec::{Handler, HookSpec, Origin, is_original_event};
use super::toolmap;
use super::{
    HookDef, HookOutcome, PreToolUse, PromptSubmit, cap_tool_output, exit_warning,
    parse_updated_args,
};
use crate::agent::{AgentEvent, PermissionAnswer, PermissionGate};
use crate::config::Mode;
use crate::permissions::{Decision, Policy, Rule};
use crate::tools::ToolAccess;

/// Services a hook handler may need that the engine does not own: a model for
/// `prompt` hooks, a subagent for `agent` hooks, an MCP server for
/// `mcp_tool` hooks. The agent builder installs one; without it those three
/// handler types are skipped with a warning.
#[async_trait]
pub trait HookBackend: Send + Sync {
    /// One model call; returns the reply text.
    async fn ask_model(&self, prompt: &str, model: Option<&str>) -> Result<String, String>;
    /// A read-only subagent run; returns its final report.
    async fn run_agent(&self, prompt: &str, model: Option<&str>) -> Result<String, String>;
    /// Call a tool on an MCP server; returns the text and whether it errored.
    async fn call_mcp(
        &self,
        server: &str,
        tool: &str,
        input: Value,
    ) -> Result<(String, bool), String>;
}

/// A [`HookSpec`] with its matcher compiled.
pub(super) struct CompiledHook {
    pub(super) spec: HookSpec,
    matcher: Matcher,
    condition: Option<Rule>,
}

/// How one hook finished.
pub(super) enum RunResult {
    Exited {
        code: i32,
        stdout: String,
        stderr: String,
    },
    /// Spawn failure, wait failure, timeout, or a missing backend.
    Failed(String),
}

/// What the hooks for one event decided, combined.
#[derive(Debug, Default)]
pub struct Outcome {
    /// The action is refused, with this reason.
    pub block: Option<String>,
    /// A hook said `continue: false`; the turn should end with this note.
    pub halt: Option<String>,
    /// Text for the model: `additionalContext` and, on the events where
    /// plain stdout counts, stdout.
    pub context: Vec<String>,
    /// Replacement tool arguments, in Wizard's shape.
    pub updated_input: Option<Value>,
    /// The strongest `permissionDecision` (deny over ask over allow).
    pub verdict: Option<Verdict>,
    pub verdict_reason: Option<String>,
    /// `PermissionRequest` answer.
    pub permission_request: Option<PermissionRequestDecision>,
    /// `systemMessage` notes for the user.
    pub system_messages: Vec<String>,
    /// Each hook's `hookSpecificOutput`, for event-specific fields.
    pub specific: Vec<Value>,
}

impl Outcome {
    /// The context lines joined for appending to a message.
    pub fn context_text(&self) -> Option<String> {
        (!self.context.is_empty()).then(|| self.context.join("\n"))
    }

    /// First value of `key` found in any hook's `hookSpecificOutput`.
    pub fn specific_str(&self, key: &str) -> Option<String> {
        self.specific
            .iter()
            .find_map(|v| v.get(key).and_then(Value::as_str).map(str::to_string))
    }
}

/// One event about to be fired.
pub struct Fire<'a> {
    pub event: HookEvent,
    /// The tool and its arguments, on tool events.
    pub tool: Option<(&'a str, &'a Value)>,
    /// What a matcher is tested against on other events (`startup`, a
    /// trigger, a command name, ...).
    pub subject: Option<&'a str>,
    /// Event fields merged into the payload.
    pub extra: Option<Value>,
    pub mode: Mode,
    pub events: Option<&'a mpsc::Sender<AgentEvent>>,
    /// Native hooks on the original events append stdout as context only
    /// where the event always did.
    pub capture: bool,
}

impl<'a> Fire<'a> {
    pub fn new(event: HookEvent, mode: Mode, events: Option<&'a mpsc::Sender<AgentEvent>>) -> Self {
        Fire {
            event,
            tool: None,
            subject: None,
            extra: None,
            mode,
            events,
            capture: false,
        }
    }

    pub fn tool(mut self, name: &'a str, args: &'a Value) -> Self {
        self.tool = Some((name, args));
        self
    }

    pub fn subject(mut self, subject: &'a str) -> Self {
        self.subject = Some(subject);
        self
    }

    pub fn extra(mut self, extra: Value) -> Self {
        self.extra = Some(extra);
        self
    }

    pub fn capture(mut self, capture: bool) -> Self {
        self.capture = capture;
        self
    }
}

/// What to do with a tool call after hooks and permission rules have spoken.
#[derive(Debug)]
pub enum Before {
    Proceed {
        args: Value,
        /// `additionalContext` from the `PreToolUse` hooks, to ride next to
        /// the tool result.
        context: Option<String>,
    },
    /// Refuse the call; the string is the tool error the model reads.
    Blocked(String),
}

/// Runs the configured hooks for lifecycle events. Built once per agent and
/// shared (via `Arc`) by the dispatcher, the agent loop, and the subagent
/// spawner, so every tool call in every mode sees the same hooks and the same
/// permission policy. With no hooks and no rules every fire point reduces to
/// an empty-Vec scan.
pub struct HookEngine {
    pub(super) hooks: Vec<CompiledHook>,
    /// Project root: hooks run here and it is the payload `cwd`.
    pub(super) cwd: PathBuf,
    /// Current session id for payloads; swapped when `/clear` starts a new
    /// session.
    session_id: Mutex<String>,
    /// Which hooks have already had an [`HookOutcome::AppendedContext`]
    /// reported, keyed by lifecycle event and handler description.
    ///
    /// A hook that appends context does so every single time it matches,
    /// which is what a context-injection hook is, so reporting each one turns
    /// a `post_tool_use` hook into a line per tool call. The first append is
    /// worth surfacing, because text entering the model's context from
    /// outside the conversation is not otherwise visible anywhere; the
    /// hundredth is not. Repeats are dropped to the log instead.
    ///
    /// Only [`HookOutcome::AppendedContext`] is deduplicated. A warning, a
    /// block and a rewrite are reported every time: those describe something
    /// that varies per call, and swallowing the second one would hide the
    /// event that mattered.
    appended: Mutex<HashSet<(&'static str, String)>>,
    backend: Mutex<Option<Arc<dyn HookBackend>>>,
    policy: RwLock<Policy>,
    /// The to-do list as of the last `todo` write, to tell new items from
    /// finished ones.
    todos: Mutex<BTreeMap<String, String>>,
}

impl HookEngine {
    /// Compile `defs` into an engine. A hook with an invalid matcher glob is
    /// dropped with a warning.
    pub fn new(defs: Vec<HookDef>, cwd: PathBuf, session_id: String) -> Self {
        Self::from_specs(
            defs.into_iter().map(HookSpec::from).collect(),
            cwd,
            session_id,
        )
    }

    /// Compile specs from any source. Identical Claude-format hooks that
    /// arrive through several settings files run once.
    pub fn from_specs(specs: Vec<HookSpec>, cwd: PathBuf, session_id: String) -> Self {
        let mut seen: Vec<(HookEvent, Option<String>, Handler)> = Vec::new();
        let mut hooks = Vec::new();
        for spec in specs {
            if spec.origin == Origin::Claude {
                let key = (spec.event, spec.matcher.clone(), spec.handler.clone());
                if seen.contains(&key) {
                    continue;
                }
                seen.push(key);
            }
            let matcher = match (&spec.matcher, spec.origin) {
                (None, _) => Ok(Matcher::Any),
                (Some(pattern), Origin::Native) => Matcher::glob(pattern),
                (Some(pattern), Origin::Claude) => Matcher::claude(Some(pattern)),
            };
            let matcher = match matcher {
                Ok(matcher) => matcher,
                Err(err) => {
                    tracing::warn!(
                        "dropping hook '{}': invalid matcher {:?}: {err}",
                        spec.handler.describe(),
                        spec.matcher
                    );
                    continue;
                }
            };
            let condition = spec.condition.as_deref().and_then(|text| {
                Rule::parse(text, &cwd)
                    .map_err(|err| tracing::warn!("ignoring hook condition: {err}"))
                    .ok()
            });
            hooks.push(CompiledHook {
                spec,
                matcher,
                condition,
            });
        }
        Self {
            hooks,
            policy: RwLock::new(Policy::new(cwd.clone())),
            cwd,
            session_id: Mutex::new(session_id),
            appended: Mutex::new(HashSet::new()),
            backend: Mutex::new(None),
            todos: Mutex::new(BTreeMap::new()),
        }
    }

    /// True when no hooks are configured (the default).
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// The configured hooks, for `/hooks`-style listings.
    pub fn specs(&self) -> impl Iterator<Item = &HookSpec> {
        self.hooks.iter().map(|hook| &hook.spec)
    }

    /// True when at least one hook listens for `event`.
    pub fn listens(&self, event: HookEvent) -> bool {
        self.hooks.iter().any(|hook| hook.spec.event == event)
    }

    /// Swap the payload session id (after `/clear` starts a new session).
    ///
    /// The append-notice filter is emptied with it: "once per session" is
    /// measured in sessions, and `/clear` gives the user a transcript with
    /// nothing in it, where the note that a hook is feeding the model text is
    /// worth making again.
    pub fn set_session_id(&self, id: String) {
        *self.session_id.lock().unwrap() = id;
        if let Ok(mut seen) = self.appended.lock() {
            seen.clear();
        }
    }

    /// Install the services `prompt`, `agent` and `mcp_tool` hooks use.
    pub fn set_backend(&self, backend: Arc<dyn HookBackend>) {
        *self.backend.lock().unwrap() = Some(backend);
    }

    /// Replace the permission policy (settings reload, `/permissions`).
    pub fn set_policy(&self, policy: Policy) {
        *self.policy.write().unwrap() = policy;
    }

    /// A copy of the policy in force.
    pub fn policy(&self) -> Policy {
        self.policy.read().unwrap().clone()
    }

    /// Change only the permission mode (`--permission-mode`, Shift+Tab).
    pub fn set_mode(&self, mode: crate::permissions::PermissionMode) {
        self.policy.write().unwrap().mode = mode;
    }

    fn session_id(&self) -> String {
        self.session_id.lock().unwrap().clone()
    }

    fn transcript_path(&self) -> Option<PathBuf> {
        let dir = crate::config::Config::sessions_dir().ok()?;
        Some(dir.join(format!("{}.jsonl", self.session_id())))
    }

    /// Tell hooks the model is asking the user something (`interview`).
    async fn elicit(&self, args: &Value, mode: Mode, events: Option<&mpsc::Sender<AgentEvent>>) {
        if !self.listens(HookEvent::Elicitation) {
            return;
        }
        let questions: Vec<String> = args
            .get("questions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|q| {
                q.get("question")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect();
        let message = questions.join("\n");
        self.elicitation("wizard", &message, args, mode, events)
            .await;
    }

    /// Diff a `todo` write against the last list seen and fire
    /// `TaskCreated` / `TaskCompleted` for what changed.
    async fn todo_events(
        &self,
        args: &Value,
        mode: Mode,
        events: Option<&mpsc::Sender<AgentEvent>>,
    ) {
        if args.get("action").and_then(Value::as_str) != Some("write")
            || !(self.listens(HookEvent::TaskCreated) || self.listens(HookEvent::TaskCompleted))
        {
            return;
        }
        let items: Vec<(String, String)> = args
            .get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|item| {
                Some((
                    item.get("content")?.as_str()?.to_string(),
                    item.get("status")?.as_str()?.to_string(),
                ))
            })
            .collect();
        let previous: BTreeMap<String, String> = std::mem::replace(
            &mut *self.todos.lock().unwrap(),
            items.iter().cloned().collect(),
        );
        for (index, (content, status)) in items.iter().enumerate() {
            let id = format!("todo-{}", index + 1);
            match previous.get(content) {
                None => {
                    let _ = self.task_created(&id, content, mode, events).await;
                }
                Some(was) if was != status && status == "completed" => {
                    let _ = self.task_completed(&id, content, mode, events).await;
                }
                _ => {}
            }
            if !previous.contains_key(content) && status == "completed" {
                let _ = self.task_completed(&id, content, mode, events).await;
            }
        }
    }

    /// `pre_tool_use` with hooks only: exit 2 vetoes the call; stdout of
    /// `{"updated_args": {...}}` or a Claude `updatedInput` rewrites the
    /// arguments (later hooks in the chain see the rewritten args).
    pub async fn pre_tool_use(
        &self,
        tool: &str,
        args: &Value,
        mode: Mode,
        events: Option<&mpsc::Sender<AgentEvent>>,
    ) -> PreToolUse {
        let out = self
            .run_event(Fire::new(HookEvent::PreToolUse, mode, events).tool(tool, args))
            .await;
        match hook_veto(&out) {
            Some(reason) => PreToolUse::Block(reason),
            None => PreToolUse::Continue(out.updated_input),
        }
    }

    /// Everything that happens before a tool runs: `PreToolUse` hooks, then
    /// the permission policy, then `PermissionRequest` hooks when a rule
    /// says ask.
    ///
    /// The order is the one Claude Code uses. A hook's `allow` does not
    /// bypass a deny or ask rule; a hook's `ask` makes the call ask even
    /// when the mode would not.
    #[allow(clippy::too_many_arguments)]
    pub async fn before_tool(
        &self,
        tool: &str,
        access: ToolAccess,
        args: &Value,
        tool_use_id: &str,
        mode: Mode,
        events: Option<&mpsc::Sender<AgentEvent>>,
    ) -> Before {
        self.before_tool_cancellable(tool, access, args, tool_use_id, mode, events, None)
            .await
    }

    /// [`Self::before_tool`], where a prompt waiting for an answer gives up
    /// when `cancel` fires.
    #[allow(clippy::too_many_arguments)]
    pub async fn before_tool_cancellable(
        &self,
        tool: &str,
        access: ToolAccess,
        args: &Value,
        tool_use_id: &str,
        mode: Mode,
        events: Option<&mpsc::Sender<AgentEvent>>,
        cancel: Option<&crate::agent::CancelHandle>,
    ) -> Before {
        let fire = Fire::new(HookEvent::PreToolUse, mode, events)
            .tool(tool, args)
            .extra(json!({ "tool_use_id": tool_use_id }));
        let out = self.run_event(fire).await;
        if let Some(reason) = hook_veto(&out) {
            return Before::Blocked(format!("blocked by pre_tool_use hook: {reason}"));
        }
        if tool == crate::tools::interview::INTERVIEW_TOOL_NAME {
            self.elicit(args, mode, events).await;
        }
        let args = out.updated_input.clone().unwrap_or_else(|| args.clone());
        let context = out.context_text();

        let policy = self.policy();
        let mut decision = policy.check(tool, access, &args);
        if decision == Decision::Allow && out.verdict == Some(Verdict::Ask) {
            decision = Decision::Ask(
                out.verdict_reason
                    .clone()
                    .unwrap_or_else(|| "a hook asked for approval".to_string()),
            );
        }
        // A hook's `allow` answers a prompt the mode raised, as it does in
        // Claude Code, but never one an `ask` rule raised.
        if matches!(decision, Decision::Ask(_))
            && out.verdict == Some(Verdict::Allow)
            && !policy.ask_rule_matches(tool, &args)
        {
            decision = Decision::Allow;
        }
        match decision {
            Decision::Allow => Before::Proceed { args, context },
            Decision::Deny(message) => {
                self.denied(tool, &args, &message, mode, events).await;
                Before::Blocked(message)
            }
            Decision::Ask(message) => {
                self.notification("permission_prompt", None, &message, mode, events)
                    .await;
                let offer = crate::permissions::offer(tool, &args, &policy.cwd);
                let fire = Fire::new(HookEvent::PermissionRequest, mode, events)
                    .tool(tool, &args)
                    .extra(json!({
                        "tool_use_id": tool_use_id,
                        "reason": message,
                        "permission_suggestions": offer.as_ref().map_or_else(Vec::new, |offer| {
                            offer
                                .rules
                                .iter()
                                .map(|rule| json!({
                                    "type": "addRules",
                                    "rules": [rule],
                                    "behavior": "allow",
                                    "destination": "localSettings",
                                }))
                                .collect()
                        }),
                    }));
                let answer = self.run_event(fire).await.permission_request;
                match answer {
                    Some(PermissionRequestDecision {
                        allow: true,
                        updated_input,
                        ..
                    }) => Before::Proceed {
                        args: updated_input
                            .map(|input| toolmap::from_claude_input(tool, &input))
                            .unwrap_or(args),
                        context,
                    },
                    Some(PermissionRequestDecision { message: text, .. }) => {
                        let text = text.unwrap_or(message);
                        self.denied(tool, &args, &text, mode, events).await;
                        Before::Blocked(text)
                    }
                    None => {
                        if let Some(answer) = self.ask_prompt_tool(tool, &args, tool_use_id).await {
                            return match answer {
                                Ok(updated) => Before::Proceed {
                                    args: updated
                                        .map(|input| toolmap::from_claude_input(tool, &input))
                                        .unwrap_or(args),
                                    context,
                                },
                                Err(text) => {
                                    self.denied(tool, &args, &text, mode, events).await;
                                    Before::Blocked(text)
                                }
                            };
                        }
                        let reply = self
                            .ask_surface(tool, &args, &message, offer.clone(), events, cancel)
                            .await;
                        match reply {
                            PermissionAnswer::Allow { remember } => {
                                if remember && let Some(offer) = offer {
                                    self.remember(tool, &offer, events).await;
                                }
                                Before::Proceed { args, context }
                            }
                            PermissionAnswer::Deny { feedback } => {
                                let text = crate::permissions::rejection(feedback.as_deref());
                                self.denied(tool, &args, &text, mode, events).await;
                                Before::Blocked(text)
                            }
                            PermissionAnswer::Unavailable => {
                                let text = if policy.claude_defaults {
                                    policy.refusal(tool, &args)
                                } else {
                                    format!(
                                        "{message} No one is available to approve it. Add an \
                                         allow rule, or answer with a PermissionRequest hook."
                                    )
                                };
                                self.denied(tool, &args, &text, mode, events).await;
                                Before::Blocked(text)
                            }
                        }
                    }
                }
            }
        }
    }

    /// `--permission-prompt-tool`: ask the named MCP tool. `None` when the
    /// flag is not set. `Ok` allows the call, with new input if the tool sent
    /// `updatedInput`; `Err` is the text the model reads.
    async fn ask_prompt_tool(
        &self,
        tool: &str,
        args: &Value,
        tool_use_id: &str,
    ) -> Option<Result<Option<Value>, String>> {
        let name = std::env::var(crate::cli_claude::env::PERMISSION_PROMPT_TOOL).ok()?;
        let Some((server, mcp_tool)) = name
            .strip_prefix("mcp__")
            .and_then(|rest| rest.split_once("__"))
        else {
            return Some(Err(format!(
                "--permission-prompt-tool {name} is not an MCP tool name (mcp__server__tool)"
            )));
        };
        let Some(backend) = self.backend() else {
            return Some(Err(
                "no MCP tools are connected to answer the prompt".to_string()
            ));
        };
        let input = json!({
            "tool_name": toolmap::claude_name(tool),
            "input": toolmap::to_claude_input(tool, args),
            "tool_use_id": tool_use_id,
        });
        Some(match backend.call_mcp(server, mcp_tool, input).await {
            Ok((text, false)) => protocol::prompt_tool_answer(&text),
            Ok((text, true)) => Err(format!("{name} failed: {text}")),
            Err(err) => Err(format!("{name} failed: {err}")),
        })
    }

    /// Put a prompt to the surface and wait for the answer. A surface that is
    /// gone, a turn that is cancelled, or no surface at all is
    /// [`PermissionAnswer::Unavailable`].
    async fn ask_surface(
        &self,
        tool: &str,
        args: &Value,
        reason: &str,
        offer: Option<crate::permissions::Offer>,
        events: Option<&mpsc::Sender<AgentEvent>>,
        cancel: Option<&crate::agent::CancelHandle>,
    ) -> PermissionAnswer {
        let Some(events) = events else {
            return PermissionAnswer::Unavailable;
        };
        let (gate, answer) = PermissionGate::open();
        let request = AgentEvent::PermissionRequest {
            tool: tool.to_string(),
            args: args.clone(),
            reason: reason.to_string(),
            offer,
            gate,
        };
        if events.send(request).await.is_err() {
            gate.cancel();
            return PermissionAnswer::Unavailable;
        }
        tokio::select! {
            biased;
            reply = answer => reply.unwrap_or(PermissionAnswer::Unavailable),
            () = events.closed() => {
                gate.cancel();
                PermissionAnswer::Unavailable
            }
            () = crate::agent::cancelled(cancel) => {
                gate.cancel();
                PermissionAnswer::Deny { feedback: None }
            }
        }
    }

    /// Apply a "don't ask again" answer: edits switch the session to
    /// `acceptEdits`, anything else becomes an allow rule, written to the
    /// project's local settings when the offer says so.
    async fn remember(
        &self,
        tool: &str,
        offer: &crate::permissions::Offer,
        events: Option<&mpsc::Sender<AgentEvent>>,
    ) {
        use crate::permissions::{PermissionMode, Scope};
        let cwd = {
            let mut policy = self.policy.write().unwrap();
            if offer.rules.is_empty() {
                if policy.mode == PermissionMode::Default {
                    policy.mode = PermissionMode::AcceptEdits;
                }
            } else {
                for rule in &offer.rules {
                    policy.allow_rule(rule, "permission prompt");
                }
            }
            policy.cwd.clone()
        };
        if offer.scope != Scope::Project || offer.rules.is_empty() {
            return;
        }
        let note = match crate::permissions::persist_allow(&cwd, &offer.rules) {
            Ok(path) => format!("{} saved to {}", offer.rules.join(", "), path.display()),
            Err(err) => format!(
                "allowed {} for this session; could not save it: {err}",
                toolmap::claude_name(tool)
            ),
        };
        if let Some(events) = events {
            let _ = events.send(AgentEvent::Notice(note)).await;
        }
    }

    async fn denied(
        &self,
        tool: &str,
        args: &Value,
        reason: &str,
        mode: Mode,
        events: Option<&mpsc::Sender<AgentEvent>>,
    ) {
        if !self.listens(HookEvent::PermissionDenied) {
            return;
        }
        let fire = Fire::new(HookEvent::PermissionDenied, mode, events)
            .tool(tool, args)
            .extra(json!({ "reason": reason }));
        self.run_event(fire).await;
    }

    /// `post_tool_use` with the tool result in the payload. Returns extra
    /// context to append to the tool result, if any hook printed some.
    pub async fn post_tool_use_with_output(
        &self,
        tool: &str,
        args: &Value,
        output: &str,
        is_error: bool,
        mode: Mode,
        events: Option<&mpsc::Sender<AgentEvent>>,
    ) -> Option<String> {
        self.after_tool(tool, args, output, is_error, "", mode, events)
            .await
    }

    /// [`Self::post_tool_use_with_output`] with the call id, and with the
    /// failure and file-change events that follow a tool run.
    #[allow(clippy::too_many_arguments)]
    pub async fn after_tool(
        &self,
        tool: &str,
        args: &Value,
        output: &str,
        is_error: bool,
        tool_use_id: &str,
        mode: Mode,
        events: Option<&mpsc::Sender<AgentEvent>>,
    ) -> Option<String> {
        let capped = cap_tool_output(output);
        let extra = json!({
            "tool_output": capped,
            "tool_response": { "output": capped, "stdout": capped },
            "is_error": is_error,
            "tool_use_id": tool_use_id,
        });
        let mut context = Vec::new();
        if !is_error {
            if tool == crate::tools::interview::INTERVIEW_TOOL_NAME {
                self.elicitation_result(
                    "wizard",
                    "accept",
                    &json!({ "answers": capped }),
                    mode,
                    events,
                )
                .await;
            } else if tool == crate::tools::todo::TODO_TOOL_NAME {
                self.todo_events(args, mode, events).await;
            }
        }
        let out = self
            .run_event(
                Fire::new(HookEvent::PostToolUse, mode, events)
                    .tool(tool, args)
                    .extra(extra)
                    .capture(true),
            )
            .await;
        context.extend(out.context);
        if is_error {
            let extra = json!({ "error": capped, "is_error": true, "tool_use_id": tool_use_id });
            let out = self
                .run_event(
                    Fire::new(HookEvent::PostToolUseFailure, mode, events)
                        .tool(tool, args)
                        .extra(extra),
                )
                .await;
            context.extend(out.context);
        } else if toolmap::class(tool) == toolmap::ToolClass::Edit
            && self.listens(HookEvent::FileChanged)
        {
            for path in toolmap::paths(tool, args) {
                let abs = crate::permissions::absolutize(&path, &self.cwd);
                let base = abs
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let extra = json!({ "file_path": abs, "event": "change" });
                self.run_event(
                    Fire::new(HookEvent::FileChanged, mode, events)
                        .subject(&base)
                        .extra(extra),
                )
                .await;
            }
        }
        (!context.is_empty()).then(|| context.join("\n"))
    }

    /// `user_prompt_submit` with the user message in the payload as `prompt`.
    /// May veto the turn or add context to the message.
    pub async fn user_prompt_submit_with_prompt(
        &self,
        prompt: &str,
        mode: Mode,
        events: Option<&mpsc::Sender<AgentEvent>>,
    ) -> PromptSubmit {
        let out = self
            .run_event(
                Fire::new(HookEvent::UserPromptSubmit, mode, events)
                    .extra(json!({ "prompt": prompt }))
                    .capture(true),
            )
            .await;
        let context = out.context_text();
        if let Some(reason) = out.block.or(out.halt) {
            return PromptSubmit::Block(reason);
        }
        PromptSubmit::Continue(context)
    }

    /// `session_start`: extra system context for the session, if any matching
    /// hook printed some.
    pub async fn session_start(
        &self,
        mode: Mode,
        events: Option<&mpsc::Sender<AgentEvent>>,
    ) -> Option<String> {
        self.session_start_from("startup", mode, events).await
    }

    /// [`Self::session_start`] with the Claude `source` (`startup`, `resume`,
    /// `clear`, `compact`).
    pub async fn session_start_from(
        &self,
        source: &str,
        mode: Mode,
        events: Option<&mpsc::Sender<AgentEvent>>,
    ) -> Option<String> {
        self.run_event(
            Fire::new(HookEvent::SessionStart, mode, events)
                .subject(source)
                .extra(json!({ "source": source }))
                .capture(true),
        )
        .await
        .context_text()
    }

    /// `session_end`: observational; output is ignored.
    pub async fn session_end(&self, mode: Mode, events: Option<&mpsc::Sender<AgentEvent>>) {
        self.run_event(
            Fire::new(HookEvent::SessionEnd, mode, events)
                .subject("other")
                .extra(json!({ "reason": "other" })),
        )
        .await;
    }

    /// `turn_end`: observational; output is ignored.
    pub async fn turn_end(&self, mode: Mode, events: Option<&mpsc::Sender<AgentEvent>>) {
        self.run_event(Fire::new(HookEvent::TurnEnd, mode, events))
            .await;
    }

    /// Fire `event` for observers. The outcome is returned for the callers
    /// that read it (a `PreCompact` block, `additionalContext`).
    pub async fn emit(
        &self,
        event: HookEvent,
        subject: Option<&str>,
        extra: Value,
        mode: Mode,
        events: Option<&mpsc::Sender<AgentEvent>>,
    ) -> Outcome {
        let mut fire = Fire::new(event, mode, events).extra(extra);
        fire.subject = subject;
        self.run_event(fire).await
    }

    /// Run the matching hooks for one event, in declaration order, and fold
    /// their answers together.
    pub async fn run_event(&self, fire: Fire<'_>) -> Outcome {
        let mut out = Outcome::default();
        let event = fire.event;
        let mut args_now: Option<Value> = None;
        let tool_name = fire.tool.map(|(name, _)| name);
        for hook in self.matching(event, tool_name, fire.subject) {
            if hook.spec.origin == Origin::Claude
                && event == HookEvent::PostToolUse
                && fire
                    .extra
                    .as_ref()
                    .and_then(|e| e.get("is_error"))
                    .and_then(Value::as_bool)
                    == Some(true)
            {
                continue;
            }
            let tool_now = fire
                .tool
                .map(|(name, args)| (name, args_now.as_ref().unwrap_or(args)));
            match (&hook.condition, hook.spec.condition.is_some(), tool_now) {
                (Some(rule), _, Some((name, args)))
                    if !rule.matches_call(name, args, &self.cwd, true) =>
                {
                    continue;
                }
                (None, true, _) | (Some(_), _, None) => continue,
                _ => {}
            }
            let payload = self.payload(hook, &fire, tool_now);
            if hook.spec.is_async {
                self.spawn_async(hook, payload);
                continue;
            }
            let run = self.execute(hook, &payload).await;
            let before_args = args_now.clone();
            if self
                .absorb(hook, &fire, tool_name, run, &mut out, &mut args_now)
                .await
            {
                break;
            }
            if args_now != before_args {
                out.updated_input = args_now.clone();
            }
        }
        out
    }

    /// Fold one hook's result into `out`. Returns true when the chain should
    /// stop (a block).
    async fn absorb(
        &self,
        hook: &CompiledHook,
        fire: &Fire<'_>,
        tool: Option<&str>,
        run: RunResult,
        out: &mut Outcome,
        args_now: &mut Option<Value>,
    ) -> bool {
        let event = fire.event;
        let events = fire.events;
        let (code, stdout, stderr) = match run {
            RunResult::Failed(why) => {
                if hook.spec.block_on_failure && event.blockable() {
                    self.report(events, event, hook, HookOutcome::Blocked(why.clone()))
                        .await;
                    out.block = Some(why);
                    return true;
                }
                self.report(events, event, hook, HookOutcome::Warning(why))
                    .await;
                return false;
            }
            RunResult::Exited {
                code,
                stdout,
                stderr,
            } => (code, stdout, stderr),
        };
        match protocol::interpret(code, &stdout, &stderr) {
            Verdicts::Block(reason, _) if event.blockable() => {
                self.report(events, event, hook, HookOutcome::Blocked(reason.clone()))
                    .await;
                out.block = Some(reason);
                true
            }
            Verdicts::Block(reason, _)
                if hook.spec.json
                    && matches!(
                        event,
                        HookEvent::PostToolUse | HookEvent::PostToolUseFailure
                    ) =>
            {
                out.context.push(format!(
                    "{} hook blocking error from command \"{}\": {reason}",
                    event.claude_name(),
                    hook.spec.handler.describe()
                ));
                self.report(events, event, hook, HookOutcome::AppendedContext)
                    .await;
                false
            }
            Verdicts::Block(_, _) => {
                self.report(
                    events,
                    event,
                    hook,
                    HookOutcome::Warning(exit_warning(code, &stderr)),
                )
                .await;
                false
            }
            Verdicts::Warn(why) => {
                if hook.spec.block_on_failure && event.blockable() {
                    self.report(events, event, hook, HookOutcome::Blocked(why.clone()))
                        .await;
                    out.block = Some(why);
                    return true;
                }
                self.report(events, event, hook, HookOutcome::Warning(why))
                    .await;
                false
            }
            Verdicts::Ok(parsed, plain) => {
                if hook.spec.json {
                    self.apply_json(hook, fire, tool, parsed, &plain, out, args_now)
                        .await
                } else {
                    self.apply_legacy(hook, fire, &plain, out, args_now).await;
                    false
                }
            }
        }
    }

    /// The frozen `hooks.toml` rules for the original events: `updated_args`
    /// on `pre_tool_use`, plain stdout as context where the event captured it.
    async fn apply_legacy(
        &self,
        hook: &CompiledHook,
        fire: &Fire<'_>,
        stdout: &str,
        out: &mut Outcome,
        args_now: &mut Option<Value>,
    ) {
        let event = fire.event;
        if event == HookEvent::PreToolUse
            && let Some(args) = parse_updated_args(stdout)
        {
            *args_now = Some(args);
            self.report(fire.events, event, hook, HookOutcome::UpdatedArgs)
                .await;
        }
        // The same hook reached through two configs (user and project both
        // pointing at one script) prints the same text twice; the model
        // needs it once.
        let repeat =
            crate::token_profile::current().safe() && out.context.iter().any(|s| s == stdout);
        if fire.capture && !stdout.is_empty() && !repeat {
            out.context.push(stdout.to_string());
            self.report(fire.events, event, hook, HookOutcome::AppendedContext)
                .await;
        }
    }

    /// The Claude decision protocol.
    #[allow(clippy::too_many_arguments)]
    async fn apply_json(
        &self,
        hook: &CompiledHook,
        fire: &Fire<'_>,
        tool: Option<&str>,
        parsed: Option<protocol::HookOutput>,
        plain: &str,
        out: &mut Outcome,
        args_now: &mut Option<Value>,
    ) -> bool {
        let event = fire.event;
        let events = fire.events;
        let Some(parsed) = parsed else {
            if !plain.is_empty() {
                if event.stdout_is_context() || (fire.capture && hook.spec.origin == Origin::Native)
                {
                    out.context.push(plain.to_string());
                    self.report(events, event, hook, HookOutcome::AppendedContext)
                        .await;
                } else {
                    tracing::debug!(
                        "hook {} ({}): stdout ignored",
                        event.name(),
                        hook.spec.handler.describe()
                    );
                }
            }
            return false;
        };
        if let Some(note) = parsed.system_message.clone() {
            out.system_messages.push(note);
        }
        if parsed.specific.is_object() {
            out.specific.push(parsed.specific.clone());
        }
        if let Some(text) = parsed.additional_context.clone() {
            out.context.push(text);
            self.report(events, event, hook, HookOutcome::AppendedContext)
                .await;
        }
        if let Some(args) = parsed.updated_args.clone() {
            *args_now = Some(args);
            self.report(events, event, hook, HookOutcome::UpdatedArgs)
                .await;
        }
        if let (Some(input), Some(tool)) = (parsed.updated_input.clone(), tool) {
            *args_now = Some(toolmap::from_claude_input(tool, &input));
            self.report(events, event, hook, HookOutcome::UpdatedArgs)
                .await;
        }
        if let Some(decision) = parsed.permission_request.clone() {
            out.permission_request = Some(decision);
        }
        if let Some(verdict) = parsed.verdict {
            let rank = |v: Option<Verdict>| match v {
                Some(Verdict::Deny) => 3,
                Some(Verdict::Ask) => 2,
                Some(Verdict::Allow) => 1,
                _ => 0,
            };
            if rank(Some(verdict)) > rank(out.verdict) {
                out.verdict = Some(verdict);
                out.verdict_reason = parsed.verdict_reason.clone();
            }
        }
        if parsed.halt && out.halt.is_none() {
            out.halt = Some(
                parsed
                    .stop_reason
                    .clone()
                    .unwrap_or_else(|| "a hook stopped the turn".to_string()),
            );
        }
        if parsed.block {
            let reason = parsed
                .reason
                .clone()
                .unwrap_or_else(|| "hook gave no reason".to_string());
            if event.blockable() {
                self.report(events, event, hook, HookOutcome::Blocked(reason.clone()))
                    .await;
                out.block = Some(reason);
                return true;
            }
            if matches!(
                event,
                HookEvent::PostToolUse | HookEvent::PostToolUseFailure
            ) {
                out.context.push(format!(
                    "{} hook blocking error from command \"{}\": {reason}",
                    event.claude_name(),
                    hook.spec.handler.describe()
                ));
            }
        }
        false
    }

    /// The configured hooks for `event` whose matcher accepts this call.
    fn matching<'a>(
        &'a self,
        event: HookEvent,
        tool: Option<&'a str>,
        subject: Option<&'a str>,
    ) -> impl Iterator<Item = &'a CompiledHook> + 'a {
        self.hooks.iter().filter(move |hook| {
            if hook.spec.event != event {
                return false;
            }
            if matches!(hook.matcher, Matcher::Any) {
                return true;
            }
            if let Some(name) = tool {
                let claude = toolmap::claude_name(name);
                return hook.matcher.is_match(name, &[&claude]);
            }
            // A matcher on a non-tool event means something only for Claude
            // hooks and for events added after the original six; on the
            // original ones `hooks.toml` always ignored it.
            let meaningful = event.has_matcher()
                && (hook.spec.origin == Origin::Claude || !is_original_event(event));
            match (meaningful, subject) {
                (true, Some(subject)) => hook.matcher.is_match(subject, &[]),
                _ => true,
            }
        })
    }

    /// The JSON a hook reads on stdin. `hooks.toml` hooks keep Wizard's field
    /// names (`event`, `tool_name`, `args`); Claude-format hooks see Claude's
    /// (`tool_name` is `Bash`, `tool_input` carries `file_path`). Both get
    /// both sets of common fields, so a script written for either reads the
    /// same payload.
    fn payload(&self, hook: &CompiledHook, fire: &Fire<'_>, tool: Option<(&str, &Value)>) -> Value {
        let event = fire.event;
        let mut payload = json!({
            "session_id": self.session_id(),
            "transcript_path": self.transcript_path(),
            "cwd": self.cwd,
            "hook_event_name": event.claude_name(),
            "permission_mode": self.policy.read().unwrap().mode.as_str(),
        });
        let map = payload.as_object_mut().expect("payload is an object");
        match (hook.spec.origin, tool) {
            (Origin::Native, Some((name, args))) => {
                map.insert("tool_name".into(), json!(name));
                map.insert("args".into(), args.clone());
                map.insert("tool_input".into(), toolmap::to_claude_input(name, args));
            }
            (Origin::Native, None) => {
                map.insert("tool_name".into(), Value::Null);
                map.insert("args".into(), Value::Null);
            }
            (Origin::Claude, Some((name, args))) => {
                map.insert("tool_name".into(), json!(toolmap::claude_name(name)));
                map.insert("tool_input".into(), toolmap::to_claude_input(name, args));
                map.insert("wizard_tool_name".into(), json!(name));
            }
            (Origin::Claude, None) => {}
        }
        if hook.spec.origin == Origin::Native {
            map.insert("event".into(), json!(event.name()));
            map.insert("mode".into(), json!(fire.mode.to_string()));
        }
        if let Some(Value::Object(fields)) = &fire.extra {
            map.extend(fields.clone());
        }
        payload
    }

    /// Run a hook without waiting for it. Its result is dropped; this is
    /// for observers that must not slow the agent.
    fn spawn_async(&self, hook: &CompiledHook, payload: Value) {
        let Handler::Command { .. } = &hook.spec.handler else {
            return;
        };
        let spec = hook.spec.clone();
        let cwd = self.cwd.clone();
        tokio::spawn(async move {
            let _ = run_command(&spec, &cwd, &payload).await;
        });
    }

    /// Run one hook to completion with its timeout enforced.
    pub(super) async fn execute(&self, hook: &CompiledHook, payload: &Value) -> RunResult {
        let timeout = Duration::from_secs(
            hook.spec
                .timeout_secs
                .unwrap_or_else(|| hook.spec.handler.default_timeout()),
        );
        let work = async {
            match &hook.spec.handler {
                Handler::Command { .. } => run_command(&hook.spec, &self.cwd, payload).await,
                Handler::Http {
                    url,
                    headers,
                    allowed_env_vars,
                } => run_http(url, headers, allowed_env_vars, payload).await,
                Handler::Prompt { prompt, model } => {
                    self.run_model(prompt, model.as_deref(), payload, false)
                        .await
                }
                Handler::Agent { prompt, model } => {
                    self.run_model(prompt, model.as_deref(), payload, true)
                        .await
                }
                Handler::McpTool {
                    server,
                    tool,
                    input,
                } => self.run_mcp(server, tool, input, payload).await,
            }
        };
        match tokio::time::timeout(timeout, work).await {
            Ok(result) => result,
            Err(_) => RunResult::Failed(format!("timed out after {}s", timeout.as_secs())),
        }
    }

    fn backend(&self) -> Option<Arc<dyn HookBackend>> {
        self.backend.lock().unwrap().clone()
    }

    async fn run_model(
        &self,
        prompt: &str,
        model: Option<&str>,
        payload: &Value,
        agent: bool,
    ) -> RunResult {
        let Some(backend) = self.backend() else {
            return RunResult::Failed("no model is attached to run this hook".to_string());
        };
        let arguments = payload.to_string();
        let full = if prompt.contains("$ARGUMENTS") {
            prompt.replace("$ARGUMENTS", &arguments)
        } else {
            format!("{prompt}\n\n{arguments}")
        };
        let full = format!(
            "{full}\n\nAnswer with one JSON object: {{\"ok\": true}} to allow, or \
             {{\"ok\": false, \"reason\": \"...\"}} to object."
        );
        let reply = if agent {
            backend.run_agent(&full, model).await
        } else {
            backend.ask_model(&full, model).await
        };
        match reply {
            Ok(text) => match parse_verdict(&text) {
                Some((true, _)) => RunResult::Exited {
                    code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                },
                Some((false, reason)) => RunResult::Exited {
                    code: 2,
                    stdout: String::new(),
                    stderr: reason,
                },
                None => RunResult::Failed("the model did not answer with {\"ok\": ...}".into()),
            },
            Err(why) => RunResult::Failed(why),
        }
    }

    async fn run_mcp(&self, server: &str, tool: &str, input: &Value, payload: &Value) -> RunResult {
        let Some(backend) = self.backend() else {
            return RunResult::Failed("no MCP connection is attached to run this hook".to_string());
        };
        let input = substitute(input, payload);
        match backend.call_mcp(server, tool, input).await {
            Ok((text, false)) => RunResult::Exited {
                code: 0,
                stdout: text,
                stderr: String::new(),
            },
            Ok((text, true)) => RunResult::Exited {
                code: 1,
                stdout: String::new(),
                stderr: text,
            },
            Err(why) => RunResult::Failed(why),
        }
    }

    /// Surface one hook's outcome. Warnings also land in the log; callers
    /// skip this for plain successes, so default behavior is unchanged.
    ///
    /// A repeated [`HookOutcome::AppendedContext`] from the same hook is
    /// logged rather than surfaced; see [`HookEngine::appended`].
    pub(super) async fn report(
        &self,
        events: Option<&mpsc::Sender<AgentEvent>>,
        event: HookEvent,
        hook: &CompiledHook,
        outcome: HookOutcome,
    ) {
        let command = hook.spec.handler.describe();
        if let HookOutcome::Warning(why) = &outcome {
            tracing::warn!("hook {} ({command}): {why}", event.name());
        }
        if outcome == HookOutcome::AppendedContext && !self.first_append(event, &command) {
            tracing::debug!(
                "hook {} ({command}): appended context (already reported this session)",
                event.name(),
            );
            return;
        }
        if let Some(events) = events {
            let _ = events
                .send(AgentEvent::HookFired {
                    event: event.name().to_string(),
                    command,
                    outcome,
                })
                .await;
        }
    }

    /// Claim the one reportable append for this hook: `true` the first time
    /// `event`/`hook` appends context, `false` for every append after it.
    ///
    /// Claiming and testing are the same operation on purpose. Two tool calls
    /// can dispatch concurrently (a subagent's and the main loop's share this
    /// engine through an `Arc`), and a check followed by a separate insert
    /// would let both see an empty set and both report.
    ///
    /// A poisoned lock is treated as "already reported". The set is a noise
    /// filter, and a panic somewhere else in the process is not a reason to
    /// start printing a line this exists to suppress.
    fn first_append(&self, event: HookEvent, command: &str) -> bool {
        self.appended
            .lock()
            .is_ok_and(|mut seen| seen.insert((event.name(), command.to_string())))
    }

    /// Commands of the hooks that would fire for `event`/`tool` (tests).
    #[cfg(test)]
    pub(super) fn matching_commands(&self, event: HookEvent, tool: Option<&str>) -> Vec<String> {
        self.matching(event, tool, None)
            .map(|hook| hook.spec.handler.describe())
            .collect()
    }
}

/// Why the hooks refused a call, if they did.
fn hook_veto(out: &Outcome) -> Option<String> {
    if let Some(reason) = &out.block {
        return Some(reason.clone());
    }
    if out.verdict == Some(Verdict::Deny) {
        return Some(
            out.verdict_reason
                .clone()
                .unwrap_or_else(|| "hook denied the call".to_string()),
        );
    }
    out.halt.clone()
}

/// Find `{"ok": bool, "reason": ".."}` in a model reply.
fn parse_verdict(text: &str) -> Option<(bool, String)> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    let value: Value = serde_json::from_str(text.get(start..=end)?).ok()?;
    let ok = value.get("ok")?.as_bool()?;
    let reason = value
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("the hook's model objected")
        .to_string();
    Some((ok, reason))
}

/// Replace `${path}` in string values with the payload field at that path
/// (`${tool_input.command}`); an unknown path becomes an empty string.
fn substitute(input: &Value, payload: &Value) -> Value {
    match input {
        Value::String(text) => {
            let mut out = String::new();
            let mut rest = text.as_str();
            while let Some(start) = rest.find("${") {
                out.push_str(&rest[..start]);
                let Some(end) = rest[start..].find('}') else {
                    out.push_str(&rest[start..]);
                    rest = "";
                    break;
                };
                let path = &rest[start + 2..start + end];
                let found = path
                    .split('.')
                    .try_fold(payload, |value, key| value.get(key));
                match found {
                    Some(Value::String(s)) => out.push_str(s),
                    Some(Value::Null) | None => {}
                    Some(other) => out.push_str(&other.to_string()),
                }
                rest = &rest[start + end + 1..];
            }
            out.push_str(rest);
            Value::String(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(|v| substitute(v, payload)).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), substitute(v, payload)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Run a command hook: payload on stdin, stdout and stderr captured.
///
/// The write goes inside the caller's timeout and runs concurrently with the
/// wait. A pipe buffers 64 KiB on Linux and a `pre_tool_use` payload carries
/// the tool's arguments, so a `write_file` of any size exceeds that on the
/// first hook that fires. Past the buffer the write blocks until the hook
/// reads, and a hook that never reads stdin never does; written before the
/// wait, that is an unbounded block before `timeout_secs` has started.
/// Joining the write with the wait also closes the other half: a hook that
/// reads stdin but prints more than a pipe's worth first would otherwise
/// deadlock against a writer that is not draining its stdout.
async fn run_command(spec: &HookSpec, cwd: &std::path::Path, payload: &Value) -> RunResult {
    let Handler::Command { command, args } = &spec.handler else {
        return RunResult::Failed("not a command hook".to_string());
    };
    // Through the platform shell, not a hand-written `sh -c`: hooks are
    // command lines (`cargo fmt && git add -u`), and which interpreter reads
    // them is a platform decision that has to match the one
    // `crate::agent::prompts` tells the model about.
    let mut process = match args {
        Some(args) => {
            let mut process = tokio::process::Command::new(command);
            process.args(args);
            process
        }
        None => crate::platform::shell::tokio_command(command),
    };
    process
        .current_dir(cwd)
        .env("CLAUDE_PROJECT_DIR", cwd)
        .env("WIZARD_PROJECT_DIR", cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(err) => return RunResult::Failed(format!("spawn failed: {err}")),
    };
    let stdin = child.stdin.take();
    let bytes = payload.to_string().into_bytes();
    let write = async {
        if let Some(mut stdin) = stdin {
            // A hook that never reads stdin may close the pipe early; that
            // is its prerogative, not an error.
            let _ = stdin.write_all(&bytes).await;
        }
    };
    match tokio::join!(write, child.wait_with_output()).1 {
        Ok(output) => RunResult::Exited {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        },
        Err(err) => RunResult::Failed(format!("wait failed: {err}")),
    }
}

/// POST the payload to a URL. Header values may name environment variables
/// (`$TOKEN`, `${TOKEN}`), but only those listed in `allowed_env_vars`; any
/// other name expands to nothing, so a project file cannot read your
/// environment into a header.
async fn run_http(
    url: &str,
    headers: &BTreeMap<String, String>,
    allowed_env_vars: &[String],
    payload: &Value,
) -> RunResult {
    let client = reqwest::Client::new();
    let mut request = client.post(url).json(payload);
    for (name, value) in headers {
        request = request.header(name, expand_env(value, allowed_env_vars));
    }
    match request.send().await {
        Ok(response) => {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            if status.is_success() {
                RunResult::Exited {
                    code: 0,
                    stdout: body,
                    stderr: String::new(),
                }
            } else {
                RunResult::Exited {
                    code: 1,
                    stdout: String::new(),
                    stderr: format!("HTTP {status}"),
                }
            }
        }
        Err(err) => RunResult::Failed(format!("request failed: {err}")),
    }
}

fn expand_env(value: &str, allowed: &[String]) -> String {
    let mut out = String::new();
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        let braced = chars.peek() == Some(&'{');
        if braced {
            chars.next();
        }
        let mut name = String::new();
        while let Some(&n) = chars.peek() {
            if n.is_ascii_alphanumeric() || n == '_' {
                name.push(n);
                chars.next();
            } else {
                break;
            }
        }
        if braced && chars.peek() == Some(&'}') {
            chars.next();
        }
        if allowed.iter().any(|a| a == &name) {
            out.push_str(&std::env::var(&name).unwrap_or_default());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitution_reads_payload_paths() {
        let payload = json!({"tool_name": "Bash", "tool_input": {"command": "ls"}, "n": 3});
        let input =
            json!({"a": "run ${tool_input.command} via ${tool_name}", "b": ["${n}", "${nope}"]});
        assert_eq!(
            substitute(&input, &payload),
            json!({"a": "run ls via Bash", "b": ["3", ""]})
        );
    }

    #[test]
    fn header_expansion_only_reads_listed_variables() {
        // SAFETY: a variable no other test reads.
        unsafe { std::env::set_var("WIZARD_HOOK_TEST_TOKEN", "secret") };
        let allowed = vec!["WIZARD_HOOK_TEST_TOKEN".to_string()];
        assert_eq!(
            expand_env("Bearer $WIZARD_HOOK_TEST_TOKEN", &allowed),
            "Bearer secret"
        );
        assert_eq!(
            expand_env("Bearer ${WIZARD_HOOK_TEST_TOKEN}!", &allowed),
            "Bearer secret!"
        );
        assert_eq!(expand_env("Bearer $WIZARD_HOOK_TEST_TOKEN", &[]), "Bearer ");
        assert_eq!(expand_env("$HOME", &allowed), "");
    }

    #[test]
    fn model_verdicts_are_found_in_chatter() {
        assert_eq!(
            parse_verdict("sure: {\"ok\": true} done"),
            Some((true, "the hook's model objected".into()))
        );
        assert_eq!(
            parse_verdict("{\"ok\": false, \"reason\": \"tests not run\"}"),
            Some((false, "tests not run".into()))
        );
        assert_eq!(parse_verdict("no json"), None);
        assert_eq!(parse_verdict("{\"ok\": \"yes\"}"), None);
    }
}
