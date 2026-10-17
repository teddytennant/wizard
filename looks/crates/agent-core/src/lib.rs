//! Backend-neutral model of an agent session.
//!
//! A frontend (`openw`, `openc`) talks to a [`Backend`] only through
//! [`Request`] going in and [`Event`] coming out. `backend-wizard` maps
//! `wizard acp` onto this; `backend-claude` maps `claude -p` stream-json onto it.
//! Frontends never see protocol JSON.

pub mod mock;
pub mod procs;
pub mod transcript;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct SlashCommand {
    /// Without the leading slash.
    pub name: String,
    pub description: String,
    /// Hint for the argument, e.g. `<provider>/<model>`. Empty when it takes none.
    pub input_hint: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ModelOption {
    /// Backend id, e.g. `xai-oauth/grok-4.6` or `claude-opus-5-5`.
    pub id: String,
    /// Human name, e.g. `grok-4.6`.
    pub name: String,
    /// Provider label, e.g. `xai-oauth`. Empty when the backend has no notion of one.
    pub provider: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct SessionInfo {
    pub id: String,
    pub title: String,
    pub cwd: String,
    /// Unix seconds of the last update, 0 if unknown.
    pub updated: i64,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Read,
    Edit,
    Search,
    Execute,
    Fetch,
    Think,
    #[default]
    Other,
}

/// The output of a `Failed` call that never finished: a replayed session whose transcript has a
/// call with no result, or one the user stopped. Frontends show it as interrupted, not as a
/// success or a refusal.
pub const INTERRUPTED_OUTPUT: &str = "interrupted before it finished";

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    #[default]
    Pending,
    Running,
    Completed,
    Failed,
}

/// A file change a tool made or proposes. `old` is `None` for a new file.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct FileDiff {
    pub path: String,
    pub old: Option<String>,
    pub new: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    /// Backend tool name, e.g. `execute`, `read_file`, `Bash`.
    pub name: String,
    pub kind: ToolKind,
    /// Short human title the backend gave it (may be empty).
    pub title: String,
    /// Raw input arguments (best effort; `Null` if unknown).
    pub input: serde_json::Value,
    pub status: ToolStatus,
    /// Text output, possibly truncated by the backend.
    pub output: Option<String>,
    pub diff: Option<FileDiff>,
    /// Set when the call belongs to a subagent run.
    pub parent_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Todo {
    pub text: String,
    pub status: TodoStatus,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    /// Tokens currently in context, when the backend reports it.
    pub context_tokens: u64,
    /// Context window size, 0 if unknown.
    pub context_window: u64,
    /// Running cost in USD, `None` if unknown.
    pub cost_usd: Option<f64>,
}

/// A tool call waiting for a person's decision. Answer it with [`Request::Decide`] using the
/// same `id`; the backend holds the call until then.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct PermissionRequest {
    pub id: String,
    /// Backend tool name, e.g. `Bash` or `Edit`.
    pub tool: String,
    #[serde(default)]
    pub kind: ToolKind,
    /// Short target: the path, the first line of the command.
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub input: serde_json::Value,
    /// The change an edit would make, when the tool is an edit.
    #[serde(default)]
    pub diff: Option<FileDiff>,
    /// What [`DecideScope::Always`] would allow in the future, e.g. `cargo test *` or `src/**`.
    /// Empty when the backend has no such rule, in which case the frontend hides the option.
    #[serde(default)]
    pub rule: String,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DecideScope {
    /// This call only.
    #[default]
    Once,
    /// Also allow what [`PermissionRequest::rule`] describes for the rest of the session.
    Always,
}

/// What rewinding to the start of a user message would undo. Answer to
/// [`Request::RewindPreview`]; `turn` counts user messages from 0 as the session shows them.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct RewindPreview {
    pub turn: usize,
    /// Files can be put back to how they were before that message.
    pub can_rewind_files: bool,
    /// Paths that would change, as the backend reports them.
    pub files: Vec<String>,
    pub insertions: usize,
    pub deletions: usize,
    /// The conversation can be cut back to just before that message.
    pub can_rewind_conversation: bool,
    /// Why one of the two is not possible, empty when both are.
    pub note: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    Cancelled,
    MaxTurns,
    Error,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    Info,
    Warn,
    Error,
}

/// One entry of a replayed transcript (session load).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum HistoryItem {
    User(String),
    Assistant(String),
    Thought(String),
    Tool(ToolCall),
}

/// Backend-reported selectable settings that are not the model.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Config {
    pub model: String,
    pub models: Vec<ModelOption>,
    /// Reasoning effort / thought level, e.g. `high`. Empty if unsupported.
    pub effort: String,
    pub efforts: Vec<String>,
    /// Agent mode, e.g. `genie`/`sovereign` or `default`/`plan`. Empty if unsupported.
    pub mode: String,
    pub modes: Vec<String>,
    pub cwd: String,
    /// Backend name and version for the footer, e.g. `wizard 3.7.1`.
    pub backend: String,
}

impl Config {
    /// The backend's own version, the last word of `backend` (`wizard 3.8.12` gives `3.8.12`).
    /// Empty until the backend has said, never a look's crate version.
    pub fn version(&self) -> &str {
        self.backend.rsplit(' ').next().unwrap_or("")
    }
}

/// The look a `/ui <name>` notice from `wizard acp` says it saved, if `text` is that notice
/// (`saved [ui] skin = "grok". ...`). A look quits when this names a look other than itself:
/// the `wizard` that started it reads `[ui] skin` on the way out and starts the new one.
pub fn switched_look(text: &str) -> Option<&str> {
    let rest = text.trim_start().strip_prefix("saved [ui] skin = \"")?;
    let (name, _) = rest.split_once('"')?;
    (!name.is_empty()).then_some(name)
}

/// What puts wizard back in the plan state the look `/ui` left was in: `/omakase` or `/plan`,
/// from the `WIZARD_HANDOFF` wizard starts this look with. `None` when it was not planning.
pub fn handed_plan() -> Option<&'static str> {
    let handoff: serde_json::Value =
        serde_json::from_str(&std::env::var("WIZARD_HANDOFF").ok()?).ok()?;
    let on = |key: &str| handoff.get(key).and_then(serde_json::Value::as_bool) == Some(true);
    if on("omakase") {
        Some("/omakase")
    } else if on("plan") {
        Some("/plan")
    } else {
        None
    }
}

/// Whether wizard is in plan mode after a `/plan` or `/omakase` notice, if `text` is one. Leaving
/// omakase goes back to plan mode, not out of it.
pub fn plan_mode(text: &str) -> Option<bool> {
    let text = text.trim_start();
    if text.starts_with("plan mode on") || text.starts_with("omakase") {
        Some(true)
    } else if text.starts_with("plan mode off") {
        Some(false)
    } else {
        None
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum Event {
    /// Backend is up; `session_id` is the active session.
    Ready {
        session_id: String,
        config: Config,
    },
    Commands(Vec<SlashCommand>),
    ConfigChanged(Config),
    Sessions(Vec<SessionInfo>),
    /// Replay of an existing session, sent before the UI shows it.
    History {
        session_id: String,
        items: Vec<HistoryItem>,
    },
    TurnStart,
    TextDelta(String),
    ThoughtDelta(String),
    /// New call, or a full replacement of one already announced (same id).
    Tool(ToolCall),
    Todos(Vec<Todo>),
    Usage(Usage),
    TurnEnd(StopReason),
    /// Out-of-band text: slash-command output, retries, warnings.
    Notice {
        level: NoticeLevel,
        text: String,
    },
    /// Backend process died or the protocol broke. The frontend should say so and offer restart.
    Fatal(String),
    /// A tool wants to run and needs a decision; see [`PermissionRequest`].
    Permission(PermissionRequest),
    /// Answer to [`Request::RewindPreview`].
    RewindPreview(RewindPreview),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum Request {
    /// Plain prompt or `/command ...`; the backend decides which it is.
    Prompt(String),
    Cancel,
    NewSession,
    LoadSession(String),
    ListSessions,
    SetModel(String),
    SetEffort(String),
    SetMode(String),
    /// Ask for the current commands/config again.
    Refresh,
    Shutdown,
    /// Answer an [`Event::Permission`]. `note` is told to the model on a denial.
    Decide {
        id: String,
        allow: bool,
        #[serde(default)]
        scope: DecideScope,
        #[serde(default)]
        note: String,
    },
    /// Stop what is running and send this text next. Backends without a better mechanism
    /// treat it as `Cancel` followed by `Prompt`.
    Steer(String),
    /// A prompt with images attached. Backends that cannot take images treat it as `Prompt(text)`.
    PromptWith {
        text: String,
        images: Vec<ImageData>,
    },
    /// Answer a question the model asked through a [`PermissionRequest`] whose `tool` is
    /// `AskUserQuestion`: `(question, answer)` pairs, several picks joined with `, `. The
    /// backend lets the call through with the answers filled in.
    Answer {
        id: String,
        answers: Vec<(String, String)>,
    },
    /// Ask what [`Request::Rewind`] to this user message would undo, without doing it.
    RewindPreview {
        turn: usize,
    },
    /// Go back to just before the `turn`th user message (0 based, as the session shows them):
    /// put files back, cut the conversation back, or both. A cut conversation arrives as a
    /// fresh [`Event::History`] under a new session id; the old session stays on disk.
    Rewind {
        turn: usize,
        conversation: bool,
        files: bool,
    },
}

/// An image sent along with a prompt.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ImageData {
    /// `image/png`, `image/jpeg`, `image/gif` or `image/webp`.
    pub media_type: String,
    /// Standard base64 of the file bytes.
    pub base64: String,
}

/// What a frontend holds. Dropping it shuts the backend down.
pub struct BackendHandle {
    pub tx: UnboundedSender<Request>,
    pub rx: UnboundedReceiver<Event>,
}

/// Implemented once per backend crate: `spawn(cwd, resume)` starts the process
/// and returns the channel pair. Must be callable from inside a tokio runtime.
pub trait Backend {
    fn spawn(cwd: std::path::PathBuf, resume: Option<String>) -> anyhow::Result<BackendHandle>;
}

/// Run a backend's driver task so that a panic in it reaches the UI. A bare `tokio::spawn`
/// swallows the panic: the task is gone, the event channel closes with nothing said, and the
/// frontend sits there drawing a session that can no longer answer.
pub fn spawn_supervised<F>(name: &'static str, ev_tx: UnboundedSender<Event>, fut: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let inner = tokio::spawn(fut);
    tokio::spawn(async move {
        if let Err(e) = inner.await {
            if e.is_panic() {
                let p = e.into_panic();
                let msg = p
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_string())
                    .or_else(|| p.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "no message".into());
                let _ = ev_tx.send(Event::Fatal(format!("{name} crashed: {msg}")));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_roundtrip_json() {
        let e = Event::Tool(ToolCall {
            id: "1".into(),
            name: "execute".into(),
            kind: ToolKind::Execute,
            ..Default::default()
        });
        let s = serde_json::to_string(&e).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&s).unwrap(), e);
    }

    #[tokio::test]
    async fn a_panicking_driver_task_becomes_a_fatal_event() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        spawn_supervised("test driver", tx, async {
            let v: Vec<u8> = Vec::new();
            let _ = v[3];
        });
        let ev = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        match ev {
            Event::Fatal(m) => assert!(m.contains("test driver crashed"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_driver_that_returns_says_nothing() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        spawn_supervised("quiet", tx, async {});
        let r = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await;
        // the sender inside the supervisor is dropped when it finishes: closed, not a Fatal
        assert!(matches!(r, Ok(None) | Err(_)), "{r:?}");
    }
}

#[cfg(test)]
mod switched_look_tests {
    use super::switched_look;

    #[test]
    fn reads_the_look_from_the_saved_notice() {
        let n = "saved [ui] skin = \"grok\". Quit this look to switch now.";
        assert_eq!(switched_look(n), Some("grok"));
    }

    #[test]
    fn other_notices_are_not_a_switch() {
        assert_eq!(switched_look("ui: codex (current)"), None);
        assert_eq!(switched_look("saved [ui] skin = \"\"."), None);
    }
}

#[cfg(test)]
mod plan_mode_tests {
    use super::plan_mode;

    #[test]
    fn reads_wizards_plan_and_omakase_notices() {
        let on = "plan mode on — the agent investigates read-only and presents a plan for approval";
        assert_eq!(plan_mode(on), Some(true));
        assert_eq!(plan_mode("plan mode off"), Some(false));
        assert_eq!(plan_mode("omakase on — chef's choice"), Some(true));
        assert_eq!(plan_mode("omakase off — back to plan mode"), Some(true));
        assert_eq!(plan_mode("switched to genie mode"), None);
    }
}
