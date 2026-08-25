//! The three things this phase has to prove, and the reason each one is here.
//!
//! The unit tests beside each module cover their own rules. These three are the
//! ones that only mean something end to end:
//!
//! 1. **A live turn reaches the window.** Driven through the real
//!    [`TaskManager`] against a scripted provider on loopback, so the path under
//!    test is agent → `AgentEvent` → tap → [`TranscriptModel`] → blocks, with
//!    nothing stubbed in the middle. If the tap is ever wired to the wrong
//!    place, or the events stop being the ones the transcript folds, this is
//!    what notices.
//!
//! 2. **Selection across three kinds of block, copied.** The kill criterion for
//!    the whole workstream (`internal/v2-decisions.md` §6). Driven by
//!    synthesizing a drag into [`UserInterface`] with a **recording clipboard**,
//!    because `iced_test`'s `Simulator::simulate` hardcodes `clipboard::Null`
//!    and therefore cannot observe a copy at all — a `Simulator`-based version of
//!    this test would pass whether or not the copy happened.
//!
//! 3. **A fixture session renders the same way twice.** A structural snapshot,
//!    not a pixel one, and that is a considered choice: a PNG of shaped text is
//!    a function of the fonts installed on the machine that rendered it, and
//!    Wizard does not bundle fonts yet (`docs/native-gui.md`, Phase 2). A golden
//!    PNG would therefore be either machine-specific — failing on every
//!    developer's box — or self-seeding, which gates nothing. What is committed
//!    instead is the block structure: order, indent, chrome and text, which is
//!    where the regressions actually are. The rasterizer is exercised
//!    separately, by drawing the same session headlessly and requiring that it
//!    not panic.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iced::advanced::graphics::text::Paragraph as GraphicsParagraph;
use iced::advanced::{clipboard, renderer::Headless};
use iced::futures::executor::block_on;
use iced::{Event, Font, Pixels, Point, Size, keyboard, mouse};
use iced_test::runtime::UserInterface;
use iced_test::runtime::user_interface;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{RwLock, mpsc};

use crate::agent::{AgentEvent, DoneReason};
use crate::config::{Config, ProviderConfig, ProviderKind};
use crate::gui::settings::ConfigStore;
use crate::gui::tasks::{TaskManager, TurnRequest};
use crate::llm::ChatMessage;
use crate::mcp::McpManager;
use crate::transcript::{TranscriptItem, TranscriptModel};

use super::select::{Block, Selectable};
use super::theme::Palette;
use super::widget;

// ---------------------------------------------------------------------------
// 1. A live turn
// ---------------------------------------------------------------------------

/// An OpenAI-compatible endpoint on loopback that replays one scripted stream.
///
/// The same idea as `tests/recorded_provider.rs`, shrunk to what this file
/// needs: the point is not to test the SSE decoder (that file already does)
/// but to have a *real* provider at the far end of a *real* agent, so the turn
/// under test is a turn and not a hand-fed sequence of events.
async fn scripted_endpoint(stream: &'static str) -> String {
    scripted_endpoint_recording(stream, Arc::new(Mutex::new(Vec::new()))).await
}

/// [`scripted_endpoint`] that keeps the body of every completion request it
/// answered.
///
/// What a resumed session *sends* is the whole question when the conversation
/// came from another program: a test that only watched the reply arrive would
/// pass on an import that dropped half the history, or took the wrong branch of
/// it. The recorded body is the only place that is observable.
async fn scripted_endpoint_recording(
    stream: &'static str,
    seen: Arc<Mutex<Vec<String>>>,
) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            // Read until the headers are complete, then whatever body came with
            // them. Good enough: the client sends one request and waits.
            while let Ok(read) = socket.read(&mut buffer).await {
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                let text = String::from_utf8_lossy(&request);
                if let Some(head) = text.find("\r\n\r\n")
                    && request.len() >= head + 4 + content_length(&text[..head])
                {
                    break;
                }
            }
            let text = String::from_utf8_lossy(&request).to_string();
            if text.contains("chat/completions")
                && let Some(head) = text.find("\r\n\r\n")
            {
                seen.lock()
                    .expect("the recorded requests")
                    .push(text[head + 4..].to_string());
            }
            let body = if text.contains("chat/completions") {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{stream}",
                    stream.len()
                )
            } else {
                // The model/context probes. Answering them with an empty object
                // keeps them from being the thing the test is measuring.
                let payload = "{\"data\":[]}";
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                )
            };
            let _ = socket.write_all(body.as_bytes()).await;
            let _ = socket.flush().await;
        }
    });
    format!("http://{addr}/v1")
}

fn content_length(head: &str) -> usize {
    head.lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0)
}

/// The shared `~/.wizard/config.toml` for the duration of one test, restored
/// afterwards.
///
/// Under `cfg(test)` every path into `~/.wizard` is redirected to one temp
/// directory per *process* (`Config::wizard_dir`), which is what keeps the suite
/// off a developer's real state — and which also means this file is shared by
/// every test in the binary. A test that needs a config on disk therefore has to
/// put back what it found, and has to be the only one doing so at a time. The
/// mutex is for the second half; `Drop` is for the first, so a panicking
/// assertion still restores the file rather than leaving every later test
/// looking at a scripted provider on a port that has since closed.
struct OnDisk {
    previous: Option<String>,
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl OnDisk {
    fn write(config: &Config) -> Self {
        static LOCK: Mutex<()> = Mutex::new(());
        let guard = LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let path = Config::path().expect("a config path under the test home");
        let previous = std::fs::read_to_string(&path).ok();
        config.save().expect("write the test config");
        Self {
            previous,
            _guard: guard,
        }
    }
}

impl Drop for OnDisk {
    fn drop(&mut self) {
        let Ok(path) = Config::path() else { return };
        match &self.previous {
            Some(text) => {
                let _ = std::fs::write(&path, text);
            }
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

/// One turn, from a real agent through the real manager into the window's
/// transcript.
///
/// The scripted stream is deliberately the awkward shape: narration, then a
/// tool call. That is what makes the assertion meaningful — a tap that only
/// carried text deltas, or one wired downstream of the frame encoder, passes a
/// "did anything arrive" test and fails this one.
#[tokio::test(flavor = "multi_thread")]
async fn a_live_turn_reaches_the_window_as_transcript_items() {
    const STREAM: &str = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Removing the stale lock.\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",",
        "\"function\":{\"name\":\"list_files\",\"arguments\":\"{}\"}}]},",
        "\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n",
    );

    let base_url = scripted_endpoint(STREAM).await;
    let workspace = tempfile::tempdir().expect("workspace");

    let mut config = Config::default();
    config.providers = vec![ProviderConfig {
        name: "scripted".to_string(),
        kind: ProviderKind::OPENAI,
        base_url,
        model: "test-model".to_string(),
        api_key_env: None,
        gguf_path: None,
        usd_per_mtok_in: None,
        usd_per_mtok_out: None,
    }];
    config.active_provider = Some("scripted".to_string());
    // One model round trip: the scripted endpoint replays the same stream
    // forever, and an unbounded agent would loop on the tool call.
    config.max_steps = crate::config::StepBudget::new(1);
    // `ConfigStore::current` re-reads the file on every turn, so that Settings
    // edits land without a restart — which means the constructor's copy is not
    // what the worker builds against. The config has to be *on disk*, and under
    // `cfg(test)` that disk is the temp wizard dir `Config::wizard_dir`
    // redirects to, which the whole test binary shares. So it is written, and
    // put back the way it was, under a guard: see [`OnDisk`].
    let _config_on_disk = OnDisk::write(&config);

    let manager = TaskManager::with_registry(
        Arc::new(ConfigStore::new(config)),
        Arc::new(RwLock::new(McpManager::empty())),
        // No registry: a test run must not advertise itself as a live session.
        None,
    );
    let id = manager
        .create_task(workspace.path(), None, None)
        .expect("create the chat");
    let task = manager.get(&id).expect("the chat is live");

    // The window's side: tap the raw events and fold them exactly as `App` does.
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let _generation = task.tap(sender);

    manager
        .submit_turn(
            &id,
            TurnRequest {
                text: "clean up".to_string(),
                ..TurnRequest::default()
            },
        )
        .expect("queue the turn");

    let mut transcript = TranscriptModel::new();
    transcript.user("clean up".to_string(), Vec::new());
    let drained = tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(event) = receiver.recv().await {
            let done = matches!(event, AgentEvent::Done { .. });
            transcript.apply(&event);
            if done {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or_else(|_| {
        // What the transcript had folded by the deadline: a build that never
        // started shows an empty one, where a turn that hung mid-stream shows
        // the text it got to. "Timed out" alone would not say which.
        panic!("timed out; the transcript held: {:?}", transcript.items());
    });
    assert!(drained, "the tap closed before the turn was done");

    // The conversation, as the one shared model read it.
    let kinds: Vec<&str> = transcript
        .items()
        .iter()
        .map(|item| match item {
            TranscriptItem::User { .. } => "user",
            TranscriptItem::Text(_) => "text",
            TranscriptItem::Tool(_) => "tool",
            TranscriptItem::Notice(_) => "notice",
            TranscriptItem::Thinking(_) => "thinking",
            TranscriptItem::Images { .. } => "images",
            TranscriptItem::TurnMarker { .. } => "marker",
        })
        .collect();
    assert!(
        kinds.starts_with(&["user", "text", "tool"]),
        "the prompt, the narration and the call, in order: {kinds:?}"
    );
    let narration = transcript
        .items()
        .iter()
        .find_map(|item| match item {
            TranscriptItem::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .expect("the model's narration");
    assert_eq!(narration, "Removing the stale lock.");
    let tool = transcript
        .items()
        .iter()
        .find_map(|item| match item {
            TranscriptItem::Tool(tool) => Some(tool),
            _ => None,
        })
        .expect("the tool row");
    assert_eq!(tool.name, "list_files");
    assert!(tool.output.is_some(), "and it was answered");

    // And it draws: the same conversation as the blocks the window lays out.
    let blocks = widget::transcript::blocks(&transcript, &palette());
    let drawn: Vec<&str> = blocks.iter().map(Block::plain).collect();
    assert!(
        drawn.contains(&"clean up") && drawn.contains(&"Removing the stale lock."),
        "{drawn:?}"
    );
    assert!(
        drawn.iter().any(|text| text.contains("list_files")),
        "{drawn:?}"
    );

    manager.shutdown();
}

// ---------------------------------------------------------------------------
// 1b. A Claude Code session, opened from this window's picker
// ---------------------------------------------------------------------------

/// Exit criterion 7, end to end and headless: a Claude Code session — a
/// **branched** one — is listed by the sidebar, opened from it, and continues.
///
/// Every step is the product's own, in the order the window runs them: the
/// shared listing, the sidebar's rendering of it, the click, the import, the
/// open, and then a real turn through a real agent against a provider that
/// records what it was sent. The last part is what makes this more than a smoke
/// test — the fixture forks once, and an import that took the wrong branch, or
/// dropped half the chain, is only visible in the request body.
///
/// The one thing that is not exercised is iced's own event loop: the two
/// asynchronous steps ([`super::App::open_claude`] and
/// [`super::App::open_chat`]) run their bodies here rather than on an
/// executor, because this machine has no compositor. Everything either side of
/// them — the messages, the state transitions, the widget tree — is the real
/// one.
#[tokio::test(flavor = "multi_thread")]
async fn a_branched_claude_session_opens_from_the_picker_and_continues() {
    use crate::session_registry::{self, Origin};

    const STREAM: &str = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Carrying on.\"},",
        "\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    /// A tool call that exists only on the branch the user rewound away from.
    const ABANDONED: &str = "toolu_0000000000000000000005";
    /// And one from the branch Claude Code itself would resume.
    const RESUMED: &str = "toolu_0000000000000000000014";

    let sent = Arc::new(Mutex::new(Vec::new()));
    let base_url = scripted_endpoint_recording(STREAM, Arc::clone(&sent)).await;
    let workspace = tempfile::tempdir().expect("workspace");
    let cwd = workspace.path().display().to_string();

    // A `~/.claude/projects`-shaped tree filed under this workspace's slug,
    // which is how Claude Code itself files them.
    let claude = tempfile::tempdir().expect("claude home");
    let projects = claude.path().join("projects");
    let project = projects.join(crate::claude_session::project_slug(&cwd).name);
    std::fs::create_dir_all(&project).expect("project dir");
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude_sessions");
    for name in ["linear.jsonl", "branched.jsonl"] {
        std::fs::copy(fixtures.join(name), project.join(name)).expect("copy fixture");
    }
    let untouched = crate::claude_session::tests_support::snapshot(&projects);
    assert_eq!(untouched.len(), 2);

    let mut config = Config::default();
    config.providers = vec![ProviderConfig {
        name: "scripted".to_string(),
        kind: ProviderKind::OPENAI,
        base_url,
        model: "test-model".to_string(),
        api_key_env: None,
        gguf_path: None,
        usd_per_mtok_in: None,
        usd_per_mtok_out: None,
    }];
    config.active_provider = Some("scripted".to_string());
    config.max_steps = crate::config::StepBudget::new(1);
    let _config_on_disk = OnDisk::write(&config);

    let manager = Arc::new(TaskManager::with_registry(
        Arc::new(ConfigStore::new(config)),
        Arc::new(RwLock::new(McpManager::empty())),
        None,
    ));
    let id = manager
        .create_task(workspace.path(), None, None)
        .expect("the window's first chat");
    let task = manager.get(&id).expect("live");
    let mut app = window(Arc::clone(&manager), task, workspace.path().to_path_buf());

    // --- the listing, shared with the browser GUI -----------------------
    let rows = session_registry::claude_chats_in(&projects, &cwd);
    assert_eq!(rows.len(), 2, "both transcripts list: {rows:?}");
    let branched = rows
        .iter()
        .find(|row| match &row.origin {
            Origin::Claude { branch_points, .. } => *branch_points > 0,
            Origin::Wizard => false,
        })
        .cloned()
        .expect("the branched fixture");
    // The word the click below aims at, checked to be the branched row's alone
    // — the two fixtures are titled differently, and a selector that matched
    // both would click whichever came first and prove nothing.
    let needle = branched
        .title
        .split_whitespace()
        .next()
        .expect("a title")
        .to_string();
    assert!(
        rows.iter()
            .filter(|row| row.title.contains(&needle))
            .count()
            == 1,
        "{needle:?} has to name one row: {rows:?}"
    );

    // --- the picker -----------------------------------------------------
    let _ = super::update(
        &mut app,
        super::Message::Sidebar(super::sidebar::Message::Loaded(super::sidebar::Listing {
            workspaces: Vec::new(),
            claude_here: true,
        })),
    );
    let _ = super::update(
        &mut app,
        super::Message::Sidebar(super::sidebar::Message::ToggleClaude),
    );
    let _ = super::update(
        &mut app,
        super::Message::Sidebar(super::sidebar::Message::ClaudeLoaded(cwd.clone(), rows)),
    );

    // Drawn in the whole window, not in the widget alone: the sidebar's
    // messages have to survive the `Message::Sidebar` mapping in `view`, and a
    // Claude row has to be marked as one where a person would see it.
    let mut ui = iced_test::simulator(super::view(&app));
    assert!(ui.find("claude code").is_ok(), "the section is headed");
    assert!(ui.find("claude").is_ok(), "and the rows are tagged");
    ui.click(crate::native::probe::contains(&needle))
        .expect("click the branched session");
    let clicked = ui.into_messages().next().expect("a message");
    let super::Message::Sidebar(super::sidebar::Message::OpenClaude { source, leaf }) = clicked
    else {
        panic!("opening a Claude row is not a Select: {clicked:?}");
    };
    assert_eq!(
        leaf.as_deref(),
        Some("00000000-0000-4000-8000-000000000047"),
        "the tip Claude Code itself would resume, off the row"
    );

    // --- the import, which is `App::open_claude`'s body -----------------
    let imported = crate::claude_resume::import(&source, leaf.as_deref(), workspace.path())
        .expect("import the chosen branch");
    let _ = super::update(
        &mut app,
        super::Message::Imported(Box::new(Ok(imported.clone()))),
    );

    // --- the open, which is `App::open_chat`'s body ---------------------
    let sessions = Config::sessions_dir().expect("sessions dir");
    let session = crate::agent::session::Session::open_by_id(&sessions, &imported.id)
        .expect("open by id")
        .expect("the imported session");
    let entries = session.entries().expect("its entries");
    manager.ensure(&imported.id).expect("spawn its worker");
    let _ = super::update(
        &mut app,
        super::Message::Opened(Box::new(Ok(super::Opened {
            id: imported.id.clone(),
            cwd: workspace.path().to_path_buf(),
            model: "test-model".to_string(),
            entries,
        }))),
    );
    app.refresh();

    assert_eq!(app.task.id, imported.id, "the window is on the import");
    // The window says what it did, in the conversation it did it to. Anything
    // less leaves "did this move my Claude Code session?" unanswered.
    let notice = app
        .transcript
        .items()
        .iter()
        .find_map(|item| match item {
            TranscriptItem::Notice(text) => Some(text.clone()),
            _ => None,
        })
        .expect("the import said what it did");
    assert!(notice.contains("read, not modified"), "{notice}");
    assert!(notice.contains(&imported.source_id), "{notice}");

    // The conversation on screen is the branch that was chosen. Asserted on the
    // provider's own call ids, which are what bind a call to the result that
    // answers it and are therefore the one thing an import cannot renumber.
    let calls: Vec<&str> = app
        .transcript
        .items()
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::Tool(tool) => Some(tool.call_id.as_str()),
            _ => None,
        })
        .collect();
    assert!(calls.contains(&RESUMED), "the resumed branch: {calls:?}");
    assert!(
        !calls.contains(&ABANDONED),
        "and not the branch that was rewound away from: {calls:?}"
    );
    assert!(!app.blocks.is_empty(), "and the window drew it");

    // --- and it continues ------------------------------------------------
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let live = manager
        .get(&imported.id)
        .expect("the imported chat is live");
    let _generation = live.tap(sender);
    manager
        .submit_turn(
            &imported.id,
            TurnRequest {
                text: "carry on".to_string(),
                ..TurnRequest::default()
            },
        )
        .expect("queue a turn on the imported chat");

    let mut transcript = TranscriptModel::seed(&session.entries().expect("entries"));
    let done = tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(event) = receiver.recv().await {
            let done = matches!(event, AgentEvent::Done { .. });
            transcript.apply(&event);
            if done {
                return true;
            }
        }
        false
    })
    .await
    .expect("the turn finished");
    assert!(done, "the tap closed before the turn was done");
    assert!(
        transcript
            .items()
            .iter()
            .any(|item| matches!(item, TranscriptItem::Text(text) if text == "Carrying on.")),
        "the reply landed: {:?}",
        transcript.items()
    );

    // What the provider was actually sent: the branch that was chosen, and not
    // the one beside it. This is the assertion the DAG makes necessary — a
    // top-to-bottom read of that file would have put both in here.
    let sent = sent.lock().expect("the recorded requests").clone();
    let request = sent.first().expect("one completion request");
    assert!(request.contains(RESUMED), "the resumed branch is replayed");
    assert!(
        !request.contains(ABANDONED),
        "the abandoned branch must not be: {request}"
    );
    assert!(request.contains("carry on"), "and the new prompt is on it");

    // Nothing in this whole path wrote to Claude Code's tree.
    assert_eq!(
        crate::claude_session::tests_support::snapshot(&projects),
        untouched,
        "listing, importing and resuming must not touch ~/.claude"
    );

    manager.shutdown();
}

// ---------------------------------------------------------------------------
// 2. Selection, the acceptance bar
// ---------------------------------------------------------------------------

/// A clipboard that remembers, which is the whole reason this test drives
/// `UserInterface` by hand: `iced_test::Simulator::simulate` hardcodes
/// `clipboard::Null`, so every write through it is discarded and a copy test
/// built on it cannot fail.
#[derive(Default)]
struct Recorder {
    standard: Mutex<Option<String>>,
    primary: Mutex<Option<String>>,
}

impl clipboard::Clipboard for &Recorder {
    fn read(&self, kind: clipboard::Kind) -> Option<String> {
        match kind {
            clipboard::Kind::Standard => self.standard.lock().expect("clipboard").clone(),
            clipboard::Kind::Primary => self.primary.lock().expect("clipboard").clone(),
        }
    }

    fn write(&mut self, kind: clipboard::Kind, contents: String) {
        let slot = match kind {
            clipboard::Kind::Standard => &self.standard,
            clipboard::Kind::Primary => &self.primary,
        };
        *slot.lock().expect("clipboard") = Some(contents);
    }
}

fn palette() -> Palette {
    Palette::from_theme(&crate::theme::minimal())
}

/// A transcript with one of each of the three kinds the criterion names:
/// prose, a fenced code block, and a tool row (header plus body).
fn three_kinds() -> TranscriptModel {
    let mut model = TranscriptModel::new();
    model.apply(&AgentEvent::TextDelta(
        "Found a stale lock file and removed it.\n\n\
         ```rust\nfn main() {\n    let x = 42;\n    println!(\"{x}\");\n}\n```\n"
            .to_string(),
    ));
    model.apply(&AgentEvent::ToolStarted {
        name: "run_shell".to_string(),
        args: serde_json::json!({ "command": "rm -f .lock" }),
    });
    model.apply(&AgentEvent::ToolFinished {
        name: "run_shell".to_string(),
        output: crate::tools::ToolOutput::ok("removed 1 file"),
    });
    model
}

fn drag(from: Point, to: Point, steps: usize) -> Vec<(Event, Point)> {
    let mut events = vec![
        (
            Event::Mouse(mouse::Event::CursorMoved { position: from }),
            from,
        ),
        (
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            from,
        ),
    ];
    for step in 1..=steps {
        let ratio = step as f32 / steps as f32;
        let at = Point::new(
            from.x + (to.x - from.x) * ratio,
            from.y + (to.y - from.y) * ratio,
        );
        events.push((Event::Mouse(mouse::Event::CursorMoved { position: at }), at));
    }
    events.push((
        Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
        to,
    ));
    events
}

fn ctrl_c() -> Event {
    let key = keyboard::Key::Character("c".into());
    Event::Keyboard(keyboard::Event::KeyPressed {
        key: key.clone(),
        modified_key: key,
        physical_key: keyboard::key::Physical::Unidentified(
            keyboard::key::NativeCode::Unidentified,
        ),
        location: keyboard::Location::Standard,
        modifiers: keyboard::Modifiers::CTRL,
        repeat: false,
        text: None,
    })
}

/// A headless renderer. No window, no compositor: `iced::Renderer` is
/// tiny-skia here (no wgpu is linked at all), and the cosmic-text font system
/// it shapes through is process-wide.
fn headless() -> iced::Renderer {
    block_on(<iced::Renderer as Headless>::new(
        Font::DEFAULT,
        Pixels(15.0),
        None,
    ))
    .expect("a headless renderer needs no window")
}

/// Run a gesture over `blocks` and return what it captured and what it copied.
fn gesture(blocks: &[Block], gesture: Vec<(Event, Point)>, then: &[Event]) -> (usize, Recorder) {
    gesture_on(&mut headless(), blocks, gesture, then)
}

fn gesture_on(
    renderer: &mut iced::Renderer,
    blocks: &[Block],
    gesture: Vec<(Event, Point)>,
    then: &[Event],
) -> (usize, Recorder) {
    let element: iced::Element<'_, (), iced::Theme, iced::Renderer> =
        Selectable::new(blocks).into();
    let mut ui = UserInterface::build(
        element,
        Size::new(760.0, 4000.0),
        user_interface::Cache::default(),
        renderer,
    );

    let recorder = Recorder::default();
    let mut clipboard = &recorder;
    let mut messages = Vec::new();
    let mut captured = 0;
    let mut at = Point::ORIGIN;

    for (event, position) in gesture {
        at = position;
        let (_, statuses) = ui.update(
            std::slice::from_ref(&event),
            mouse::Cursor::Available(at),
            &mut *renderer,
            &mut clipboard,
            &mut messages,
        );
        captured += statuses
            .iter()
            .filter(|status| **status == iced::event::Status::Captured)
            .count();
    }
    for event in then {
        let (_, statuses) = ui.update(
            std::slice::from_ref(event),
            mouse::Cursor::Available(at),
            &mut *renderer,
            &mut clipboard,
            &mut messages,
        );
        captured += statuses
            .iter()
            .filter(|status| **status == iced::event::Status::Captured)
            .count();
    }
    drop(ui);
    (captured, recorder)
}

/// **The acceptance bar for the whole workstream.** A drag that begins in a
/// prose paragraph, crosses a syntax-highlighted code block, and ends in a tool
/// row; then Ctrl+C; then the clipboard has all three.
///
/// Stock iced cannot do this and the spike proved it: three stock widgets each
/// see half the gesture and none of them owns a range spanning the others.
#[test]
fn a_drag_across_prose_code_and_a_tool_row_copies_all_three() {
    let blocks = widget::transcript::blocks(&three_kinds(), &palette());
    // Prose, code, tool header, tool body — the shape the whole test rests on,
    // asserted rather than assumed so a change in `widget::transcript` fails
    // here loudly instead of quietly weakening the drag.
    assert_eq!(
        blocks.len(),
        4,
        "{:?}",
        blocks.iter().map(Block::plain).collect::<Vec<_>>()
    );

    // From the very start of the prose to well past the last block: the widget
    // clamps a drag that runs off the bottom to the end of the last block,
    // which is what a real drag to the bottom of the window does.
    let (captured, clipboard) = gesture(
        &blocks,
        drag(Point::new(2.0, 4.0), Point::new(700.0, 3000.0), 16),
        &[ctrl_c()],
    );

    assert!(captured > 0, "the widget has to own the gesture");
    let copied = clipboard.standard.lock().expect("clipboard").clone();
    let copied = copied.expect("Ctrl+C wrote to the clipboard");
    assert!(
        copied.contains("stale lock file"),
        "the prose is missing: {copied:?}"
    );
    assert!(
        copied.contains("println!"),
        "the code block is missing: {copied:?}"
    );
    assert!(
        copied.contains("run_shell"),
        "the tool row is missing: {copied:?}"
    );
    assert!(
        copied.contains("removed 1 file"),
        "the tool's output is missing: {copied:?}"
    );
    // The code block's own newlines survive, which is the thing
    // `Hit::CharOffset` would have destroyed by discarding the line index.
    assert!(
        copied.contains("fn main() {\n    let x = 42;"),
        "the code block copied as one line: {copied:?}"
    );
}

/// A selection that **ends inside** a code block ends where the mouse was, on
/// the visual line it was on.
///
/// This is the one the lossy `Hit::CharOffset` gets wrong, and it gets it wrong
/// quietly: the offset it returns is relative to the cosmic-text *buffer line*,
/// so ending on the second line of a code block reports a small number that
/// looks like a plausible offset into the block and truncates the copy near its
/// top. Every code block has newlines, so "quietly wrong" would be the normal
/// case rather than an edge one.
///
/// The y of a visual line is a function of the shaped line height, which is a
/// function of whichever monospace font the machine resolved — so rather than
/// assume a metric, this scans down the block and requires that *some* row
/// produces exactly the first two lines. Reading the block-relative offset out
/// of `Cursor::index` alone cannot produce that string at any y: the longest
/// per-line index in this block is shorter than its second line's end.
#[test]
fn a_selection_ending_on_the_second_line_of_a_code_block_ends_there() {
    let mut model = TranscriptModel::new();
    model.apply(&AgentEvent::TextDelta(
        "```rust\nfn main() {\n    let x = 42;\n    println!(\"{x}\");\n}\n```\n".to_string(),
    ));
    let blocks = widget::transcript::blocks(&model, &palette());
    assert_eq!(blocks.len(), 1, "the code block is the whole transcript");
    let code = blocks[0].plain().to_string();
    assert!(code.starts_with("fn main() {\n    let x = 42;"), "{code:?}");
    let wanted = "fn main() {\n    let x = 42;";

    let mut renderer = headless();
    let mut seen = Vec::new();
    // Far enough right to clamp to the end of whatever line the y lands on, and
    // down through the first few rows of the block.
    for row in 0..24 {
        let y = 2.0 + row as f32 * 3.0;
        let (_, clipboard) = gesture_on(
            &mut renderer,
            &blocks,
            drag(Point::new(1.0, 1.0), Point::new(740.0, y), 4),
            &[ctrl_c()],
        );
        if let Some(copied) = clipboard.standard.lock().expect("clipboard").clone() {
            seen.push(copied);
        }
    }
    assert!(
        seen.iter().any(|copied| copied == wanted),
        "no row selected exactly the first two lines; got {seen:?}"
    );
}

/// The start of a drag is where the mouse went down, not the start of the
/// block. Without this, every selection silently begins at offset 0 — which is
/// exactly what a widget that ignored the x coordinate would do, and it would
/// still pass the test above.
#[test]
fn a_drag_that_starts_mid_paragraph_copies_from_there() {
    let blocks = widget::transcript::blocks(&three_kinds(), &palette());
    let prose = blocks[0].plain().to_string();

    let (_, clipboard) = gesture(
        &blocks,
        drag(Point::new(120.0, 6.0), Point::new(700.0, 3000.0), 12),
        &[ctrl_c()],
    );
    let copied = clipboard.standard.lock().expect("clipboard").clone();
    let copied = copied.expect("Ctrl+C wrote to the clipboard");

    assert!(
        !copied.starts_with(&prose),
        "the drag began 120px in and still copied the whole paragraph: {copied:?}"
    );
    let head = copied.lines().next().unwrap_or_default();
    assert!(
        !head.is_empty() && prose.contains(head),
        "the first copied line has to be a tail of the paragraph: {head:?}"
    );
    assert!(copied.contains("run_shell"), "and still reach the tool row");
}

/// Releasing the mouse fills the X11 primary selection, which is what makes a
/// middle-click paste work without a copy step. It is a *separate* clipboard,
/// so writing the standard one instead would look right in this test's sibling
/// and be wrong on the desktop.
#[test]
fn releasing_a_drag_fills_the_primary_selection_only() {
    let blocks = widget::transcript::blocks(&three_kinds(), &palette());
    let (_, clipboard) = gesture(
        &blocks,
        drag(Point::new(2.0, 4.0), Point::new(700.0, 3000.0), 8),
        &[],
    );
    assert!(
        clipboard
            .primary
            .lock()
            .expect("clipboard")
            .as_deref()
            .is_some_and(|text| text.contains("run_shell")),
        "the primary selection was not filled"
    );
    assert!(
        clipboard.standard.lock().expect("clipboard").is_none(),
        "a drag with no Ctrl+C must not touch the standard clipboard"
    );
}

/// A click with no drag selects nothing and must not clobber whatever the user
/// already had on their clipboard with an empty string.
#[test]
fn a_bare_click_copies_nothing() {
    let blocks = widget::transcript::blocks(&three_kinds(), &palette());
    let at = Point::new(40.0, 6.0);
    let (_, clipboard) = gesture(
        &blocks,
        vec![
            (Event::Mouse(mouse::Event::CursorMoved { position: at }), at),
            (
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                at,
            ),
            (
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                at,
            ),
        ],
        &[ctrl_c()],
    );
    assert!(clipboard.standard.lock().expect("clipboard").is_none());
    assert!(clipboard.primary.lock().expect("clipboard").is_none());
}

// ---------------------------------------------------------------------------
// 3. The fixture session
// ---------------------------------------------------------------------------

/// The committed session: one `ChatMessage` per line, which is the shape a real
/// session file carries and what `TranscriptModel::seed_messages` replays.
fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/native")
        .join(name)
}

fn fixture_session() -> TranscriptModel {
    let text = std::fs::read_to_string(fixture_path("session.jsonl")).expect("the fixture session");
    let messages: Vec<ChatMessage> = text
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with("//"))
        .map(|line| serde_json::from_str(line).expect("a fixture line is one ChatMessage"))
        .collect();
    TranscriptModel::seed_messages(&messages)
}

/// One block as a stable line: everything about it that a reader would notice
/// changing, and nothing that a font or a machine could move.
fn describe_block(block: &Block) -> String {
    format!(
        "size={:<4} indent={:<4} gap={:<4} fill={} rule={} spans={:<2} | {}",
        block.size.0,
        block.indent,
        block.gap,
        if block.fill.is_some() { 'y' } else { '-' },
        if block.rule.is_some() { 'y' } else { '-' },
        block.spans.len(),
        block.plain().replace('\n', "\\n"),
    )
}

/// The snapshot. Committed, so a change to any renderer in `widget::` shows up
/// as a diff a reviewer reads rather than as a screenshot nobody compares.
///
/// To re-bless it: `WIZARD_BLESS_SNAPSHOTS=1 cargo test --features native`.
#[test]
fn a_fixture_session_renders_to_its_snapshot() {
    let blocks = widget::transcript::blocks(&fixture_session(), &palette());
    let rendered: String = blocks
        .iter()
        .map(|block| format!("{}\n", describe_block(block)))
        .collect();

    let path = fixture_path("session.snapshot");
    if std::env::var_os("WIZARD_BLESS_SNAPSHOTS").is_some() {
        std::fs::write(&path, &rendered).expect("write the snapshot");
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "{} is missing; re-bless with WIZARD_BLESS_SNAPSHOTS=1",
            path.display()
        )
    });
    assert_eq!(
        rendered, expected,
        "the fixture session renders differently; re-bless with \
         WIZARD_BLESS_SNAPSHOTS=1 if the change is intended"
    );
}

/// And it rasterizes. The structural snapshot above says nothing about the
/// draw path, which is where an out-of-bounds highlight rectangle or a
/// paragraph handed the wrong origin would show up — as a panic, in a loop that
/// takes the window with it.
#[test]
fn the_fixture_session_draws_headlessly_with_a_selection_over_it() {
    let blocks = widget::transcript::blocks(&fixture_session(), &palette());
    let mut renderer: iced::Renderer = block_on(<iced::Renderer as Headless>::new(
        Font::DEFAULT,
        Pixels(15.0),
        None,
    ))
    .expect("a headless renderer needs no window");

    let element: iced::Element<'_, (), iced::Theme, iced::Renderer> = Selectable::new(&blocks)
        .selection_color(palette().selection)
        .into();
    let size = Size::new(760.0, 900.0);
    let mut ui = UserInterface::build(
        element,
        size,
        user_interface::Cache::default(),
        &mut renderer,
    );

    let recorder = Recorder::default();
    let mut clipboard = &recorder;
    let mut messages = Vec::new();
    // Select everything, so the highlight geometry is exercised on every block
    // rather than on none.
    for (event, position) in drag(Point::new(1.0, 1.0), Point::new(750.0, 4000.0), 10) {
        let _ = ui.update(
            std::slice::from_ref(&event),
            mouse::Cursor::Available(position),
            &mut renderer,
            &mut clipboard,
            &mut messages,
        );
    }

    ui.draw(
        &mut renderer,
        &iced::Theme::Dark,
        &iced::advanced::renderer::Style {
            text_color: iced::Color::WHITE,
        },
        mouse::Cursor::Available(Point::new(10.0, 10.0)),
    );
    let pixels = renderer.screenshot(
        Size::new(size.width as u32, size.height as u32),
        1.0,
        palette().canvas,
    );
    assert_eq!(
        pixels.len(),
        (size.width as usize) * (size.height as usize) * 4,
        "the software rasterizer produced a full frame"
    );
    assert!(
        pixels
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| pixel[..3] != [0, 0, 0]),
        "every pixel is black, so nothing was drawn"
    );
}

/// The paragraph type the whole selection layer is written against. If iced
/// ever changes the concrete `Paragraph` behind the default renderer, the
/// escape hatch in `select::geometry` stops compiling — and this says which
/// assumption broke rather than leaving a wall of trait errors.
#[test]
fn the_default_renderer_shapes_the_paragraph_the_geometry_reads() {
    fn assert_shape<P: 'static>() {
        assert_eq!(
            std::any::TypeId::of::<P>(),
            std::any::TypeId::of::<GraphicsParagraph>(),
        );
    }
    assert_shape::<<iced::Renderer as iced::advanced::text::Renderer>::Paragraph>();
}

// ---------------------------------------------------------------------------
// 4. The window's own bookkeeping
// ---------------------------------------------------------------------------
/// A window that never rebuilt its blocks would render an empty transcript
/// forever; one that rebuilt them on every message would walk the whole
/// conversation per mouse move. `revision` is what tells the two apart, so
/// it is what is asserted.
#[test]
fn blocks_are_rebuilt_when_the_conversation_moves_and_not_otherwise() {
    let mut transcript = TranscriptModel::new();
    transcript.user("hello".to_string(), Vec::new());
    let palette = palette();

    let mut drawn = u64::MAX;
    let mut rebuilds = 0;
    let mut refresh = |transcript: &TranscriptModel, rebuilds: &mut usize| {
        if transcript.revision() != drawn {
            drawn = transcript.revision();
            *rebuilds += 1;
            widget::transcript::blocks(transcript, &palette)
        } else {
            Vec::new()
        }
    };

    let blocks = refresh(&transcript, &mut rebuilds);
    assert_eq!(rebuilds, 1);
    assert_eq!(blocks.len(), 1);

    let _ = refresh(&transcript, &mut rebuilds);
    assert_eq!(rebuilds, 1, "nothing moved, nothing was rebuilt");

    transcript.apply(&AgentEvent::TextDelta("hi".to_string()));
    let _ = refresh(&transcript, &mut rebuilds);
    assert_eq!(rebuilds, 2, "a streaming delta is a change");
}

/// Every ending except a clean one gets said out loud. A turn that hit its
/// step budget and stopped silently is indistinguishable from one that
/// finished, which is the failure mode this exists to prevent.
#[test]
fn every_ending_but_completion_is_described() {
    for reason in [
        DoneReason::Stopped,
        DoneReason::MaxSteps,
        DoneReason::TimeLimit,
        DoneReason::CircuitBreaker,
    ] {
        assert_ne!(super::describe(reason), "completed", "{reason:?}");
    }
    assert_eq!(super::describe(DoneReason::Completed), "completed");
}

/// A headless box is where this command is most likely to be typed by mistake,
/// and iced's own failure there is an `.expect()` with a backtrace — raised
/// after the session file has already been written. The forecast has to run
/// before any of that, and it has to agree with winit about which variables
/// count.
#[test]
#[cfg(all(unix, not(target_os = "macos")))]
fn a_missing_display_is_reported_before_anything_has_a_side_effect() {
    // This suite runs on the machine's real environment, so the assertion has
    // to hold either way round rather than mutating a process-wide variable
    // out from under every other test.
    let have_one = ["WAYLAND_DISPLAY", "WAYLAND_SOCKET", "DISPLAY"]
        .iter()
        .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()));
    match super::no_display() {
        Some(why) => {
            assert!(!have_one, "a display is set but the check refused: {why}");
            assert!(why.contains("DISPLAY"), "it names what to set: {why}");
            // The ways out, and all four of them: this used to point at
            // `wizard gui` — the browser GUI — which was the honest answer for
            // a headless box right up until that surface was deleted. A
            // message that still named it would send people at a subcommand
            // that now refuses for the same reason.
            for way in ["ssh -X", "wizard -p", "wizard acp", "wizard gateway"] {
                assert!(why.contains(way), "the way out names {way}: {why}");
            }
        }
        None => assert!(have_one, "no display is set and the check allowed it"),
    }
}

// ---------------------------------------------------------------------------
// 5. Fonts, and a real pixel snapshot
// ---------------------------------------------------------------------------

/// Register the bundled faces with the process-wide font system.
///
/// The window does this through `iced::Settings`; a headless renderer has no
/// settings, so the same bytes go in by hand. Idempotent, because the tests in
/// this file share a process and a font database.
fn load_bundled_fonts() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let mut system = iced::advanced::graphics::text::font_system()
            .write()
            .expect("the font system");
        system.load_font(super::font::INTER.into());
        system.load_font(super::font::JETBRAINS_MONO.into());
    });
}

/// **The pixel snapshot.** Phase 1 could not have this one.
///
/// A PNG of shaped text is a function of the fonts on the machine that rendered
/// it, so before the faces were bundled a golden image was either
/// machine-specific or self-seeding, and what got committed was the block
/// structure instead. With Inter and JetBrains Mono embedded and registered
/// above, every glyph in this frame comes out of the repository: the same bytes,
/// the same `wght` axis, the same tiny-skia rasterizer, the same hinting. The
/// digest is therefore a property of this tree.
///
/// It is a **hash** rather than a PNG because the image is 2.7 MB of RGBA and a
/// binary that size in git says nothing a reviewer can read; the failure below
/// says what to do about a legitimate change.
///
/// The fixture is deliberately Latin-only prose and code. A `✓` or a `──` would
/// leave the two bundled subsets and land in whatever the machine falls back
/// to, which is exactly the machine-dependence being removed here — so the tool
/// rows, which carry glyphs, are drawn by the structural snapshot above and not
/// by this one.
#[test]
fn the_bundled_fonts_rasterize_to_a_committed_digest() {
    load_bundled_fonts();
    let mut model = TranscriptModel::new();
    model.user("rename the lock file".to_string(), Vec::new());
    model.apply(&AgentEvent::TextDelta(
        "The stale lock is written by an older build.\n\n\
         ```rust\nfn main() {\n    let x = 42;\n}\n```\n"
            .to_string(),
    ));
    model.commit();

    let blocks = widget::transcript::blocks(&model, &palette());
    let mut renderer: iced::Renderer = block_on(<iced::Renderer as Headless>::new(
        super::font::SANS,
        Pixels(14.0),
        None,
    ))
    .expect("a headless renderer needs no window");

    let element: iced::Element<'_, (), iced::Theme, iced::Renderer> = Selectable::new(&blocks)
        .selection_color(palette().selection)
        .into();
    let size = Size::new(600.0, 300.0);
    let mut ui = UserInterface::build(
        element,
        size,
        user_interface::Cache::default(),
        &mut renderer,
    );
    ui.draw(
        &mut renderer,
        &iced::Theme::Dark,
        &iced::advanced::renderer::Style {
            text_color: palette().color(crate::theme::Token::Text),
        },
        mouse::Cursor::Unavailable,
    );
    let pixels = renderer.screenshot(
        Size::new(size.width as u32, size.height as u32),
        1.0,
        palette().canvas,
    );

    // The frame really has glyphs in it, so a digest of an empty canvas cannot
    // pass for a rendering.
    let ink = pixels
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|pixel| pixel[..3] != [12, 12, 14])
        .count();
    assert!(
        ink > 2_000,
        "only {ink} non-canvas pixels: the text did not render"
    );

    let digest = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(&pixels);
        format!("{:x}", hasher.finalize())
    };
    let path = fixture_path("session.pixels.sha256");
    if std::env::var_os("WIZARD_BLESS_SNAPSHOTS").is_some() {
        std::fs::write(&path, &digest).expect("write the pixel digest");
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| {
            panic!(
                "{} is missing. It is committed on purpose — a golden that seeds \
                 itself when absent is a test that can only ever pass. Re-bless \
                 with WIZARD_BLESS_SNAPSHOTS=1 once you have looked at why.",
                path.display()
            )
        })
        .trim()
        .to_string();
    assert_eq!(
        digest, expected,
        "the rasterized frame changed. If that was intended (a palette, a size, \
         a font), re-bless with WIZARD_BLESS_SNAPSHOTS=1."
    );
}

/// Nothing the window draws may reach for the system monospace.
///
/// `Font::MONOSPACE` resolves through cosmic-text's generic family to whatever
/// fontconfig nominates, which on a plain Linux box is DejaVu Sans Mono — beside
/// the JetBrains Mono this build bundles. It compiles, it renders, and the only
/// symptom is that half the literals in the window are the wrong typeface. So it
/// is asserted rather than reviewed.
#[test]
fn no_block_the_transcript_produces_uses_the_system_monospace() {
    let blocks = widget::transcript::blocks(&fixture_session(), &palette());
    for block in &blocks {
        assert_ne!(block.font, Font::MONOSPACE, "{}", block.plain());
        assert_ne!(block.font, Font::DEFAULT, "{}", block.plain());
        for span in &block.spans {
            if let Some(font) = span.font {
                assert_ne!(font, Font::MONOSPACE, "{}", block.plain());
                assert_ne!(font, Font::DEFAULT, "{}", block.plain());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 6. The console: a command that asks a question, answered from this window
// ---------------------------------------------------------------------------

/// The whole interactive-command path, end to end and through the real
/// manager: a scripted model calls `execute` on a command that reads stdin, the
/// task announces a console because [`TaskManager::attended`] built it that way,
/// this window claims the gate and types an answer, and the answer comes back
/// in the tool's own output.
///
/// Every link in that chain is one the browser GUI does not have, and each of
/// them fails differently: a manager built with `TaskManager::new` emits no
/// `ConsoleOpened` at all, a window that does not claim leaves
/// `ConsoleHost::attended` false and the command dies at its timeout, and a
/// claim that never writes leaves `read` at EOF with an empty answer. The
/// assertion on the *content* is what tells the three apart.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_prompting_command_is_answered_from_this_window() {
    const STREAM: &str = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",",
        "\"function\":{\"name\":\"execute\",\"arguments\":",
        "\"{\\\"command\\\":\\\"read name; echo hello $name\\\",\\\"timeout_secs\\\":20}\"}}]},",
        "\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n",
    );

    let base_url = scripted_endpoint(STREAM).await;
    let workspace = tempfile::tempdir().expect("workspace");
    let mut config = Config::default();
    config.providers = vec![ProviderConfig {
        name: "scripted".to_string(),
        kind: ProviderKind::OPENAI,
        base_url,
        model: "test-model".to_string(),
        api_key_env: None,
        gguf_path: None,
        usd_per_mtok_in: None,
        usd_per_mtok_out: None,
    }];
    config.active_provider = Some("scripted".to_string());
    config.max_steps = crate::config::StepBudget::new(1);
    let _config_on_disk = OnDisk::write(&config);

    // `attended`, which is the one line that separates this window from the
    // browser GUI's server on this path.
    let manager = TaskManager::attended_with_registry(
        Arc::new(ConfigStore::new(config)),
        Arc::new(RwLock::new(McpManager::empty())),
        None,
    );
    let id = manager
        .create_task(workspace.path(), None, None)
        .expect("create the chat");
    let task = manager.get(&id).expect("the chat is live");

    let (sender, mut receiver) = mpsc::unbounded_channel();
    let _generation = task.tap(sender);
    manager
        .submit_turn(
            &id,
            TurnRequest {
                text: "greet me".to_string(),
                ..TurnRequest::default()
            },
        )
        .expect("queue the turn");

    let mut transcript = TranscriptModel::new();
    let mut console: Option<super::console::Console> = None;
    let mut answered = false;
    let outcome = tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(event) = receiver.recv().await {
            // Exactly what `App::absorb` does with these three.
            match &event {
                AgentEvent::ConsoleOpened { command, gate } => {
                    console = super::console::Console::claim(*gate, command.clone());
                    if let Some(open) = &console {
                        assert!(open.line("wizard"), "the writer reached the child");
                        transcript.console_echo("wizard");
                        answered = true;
                    }
                }
                AgentEvent::ConsoleClosed { gate }
                    if console.as_ref().is_some_and(|open| open.is(*gate)) =>
                {
                    console = None;
                }
                _ => {}
            }
            let done = matches!(event, AgentEvent::Done { .. });
            transcript.apply(&event);
            if done {
                return true;
            }
        }
        false
    })
    .await
    .expect("the turn finished");
    assert!(outcome, "the tap closed before the turn was done");

    assert!(
        answered,
        "no console was announced: the task was not built attended"
    );
    assert!(console.is_none(), "the console closed with its command");

    let output = transcript
        .items()
        .iter()
        .find_map(|item| match item {
            TranscriptItem::Tool(tool) => tool.output.as_ref(),
            _ => None,
        })
        .expect("the execute row was answered");
    assert!(
        output.content.contains("hello wizard"),
        "the command did not receive what was typed: {:?}",
        output.content
    );
    // And the human's line is in the conversation, marked. A pipe does not echo
    // what you type, so without this the answer leaves no trace anywhere.
    assert!(
        transcript
            .items()
            .iter()
            .any(|item| matches!(item, TranscriptItem::Tool(tool) if tool
                .output
                .as_ref()
                .is_some_and(|out| out.content.contains("hello wizard")))),
        "the tool row carries the result"
    );

    manager.shutdown();
}

/// The other half of the same wire: a manager built the ordinary way announces
/// no console at all, so the browser GUI's behaviour is unchanged by any of
/// this. Without it, `attended` could be a no-op and the test above would still
/// pass on a manager that was interactive for everyone.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn an_unattended_manager_announces_no_console() {
    const STREAM: &str = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",",
        "\"function\":{\"name\":\"execute\",\"arguments\":",
        "\"{\\\"command\\\":\\\"read name; echo hello $name\\\",\\\"timeout_secs\\\":20}\"}}]},",
        "\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n",
    );

    let base_url = scripted_endpoint(STREAM).await;
    let workspace = tempfile::tempdir().expect("workspace");
    let mut config = Config::default();
    config.providers = vec![ProviderConfig {
        name: "scripted".to_string(),
        kind: ProviderKind::OPENAI,
        base_url,
        model: "test-model".to_string(),
        api_key_env: None,
        gguf_path: None,
        usd_per_mtok_in: None,
        usd_per_mtok_out: None,
    }];
    config.active_provider = Some("scripted".to_string());
    config.max_steps = crate::config::StepBudget::new(1);
    let _config_on_disk = OnDisk::write(&config);

    let manager = TaskManager::with_registry(
        Arc::new(ConfigStore::new(config)),
        Arc::new(RwLock::new(McpManager::empty())),
        None,
    );
    let id = manager
        .create_task(workspace.path(), None, None)
        .expect("create the chat");
    let task = manager.get(&id).expect("the chat is live");
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let _generation = task.tap(sender);
    manager
        .submit_turn(
            &id,
            TurnRequest {
                text: "greet me".to_string(),
                ..TurnRequest::default()
            },
        )
        .expect("queue the turn");

    let announced = tokio::time::timeout(Duration::from_secs(30), async {
        let mut announced = false;
        while let Some(event) = receiver.recv().await {
            if matches!(event, AgentEvent::ConsoleOpened { .. }) {
                announced = true;
            }
            if matches!(event, AgentEvent::Done { .. }) {
                break;
            }
        }
        announced
    })
    .await
    .expect("the turn finished");
    assert!(
        !announced,
        "an ordinary GUI task must keep /dev/null on fd 0"
    );
    manager.shutdown();
}

// ---------------------------------------------------------------------------
// 7. Switching which chat is on screen
// ---------------------------------------------------------------------------

/// Switching rebuilds the subscription.
///
/// `event::Feed` is identified by its **hash** — iced calls the builder again
/// only when that changes — so "the tap follows the chat" is exactly the claim
/// that the hash moves with the task id and with the generation, and does not
/// move with anything else. A `Feed` that folded in the `Arc`'s address would
/// look new on every redraw and re-tap the same task forever.
#[test]
fn the_event_feed_is_identified_by_the_chat_and_the_generation() {
    use std::hash::{Hash, Hasher};

    fn digest(feed: &super::event::Feed) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        feed.hash(&mut hasher);
        hasher.finish()
    }

    let one = crate::gui::tasks::TaskShared::new(
        "chat-one".to_string(),
        PathBuf::from("/src/a"),
        "m".to_string(),
        "genie".to_string(),
        None,
    );
    let two = crate::gui::tasks::TaskShared::new(
        "chat-two".to_string(),
        PathBuf::from("/src/b"),
        "m".to_string(),
        "genie".to_string(),
        None,
    );
    let feed = |task: &Arc<crate::gui::tasks::TaskShared>, generation| super::event::Feed {
        task: Arc::clone(task),
        generation,
    };

    let base = digest(&feed(&one, 0));
    assert_eq!(base, digest(&feed(&one, 0)), "a redraw is not a new feed");
    assert_ne!(base, digest(&feed(&two, 0)), "another chat is a new feed");
    assert_ne!(base, digest(&feed(&one, 1)), "a bumped generation rebuilds");

    // And a second handle to the same task hashes the same, which is the part
    // that would break if the pointer were folded in.
    let same = Arc::clone(&one);
    assert_eq!(base, digest(&feed(&same, 0)));
}

/// An [`App`] with no compositor behind it, on a task the caller owns.
fn window(
    manager: Arc<TaskManager>,
    task: Arc<crate::gui::tasks::TaskShared>,
    cwd: PathBuf,
) -> super::App {
    let store = Arc::new(ConfigStore::new(Config::default()));
    let settings = super::settings::Sheet::new(
        Arc::clone(&store),
        Arc::new(crate::gui::oauth::SignIn::default()),
    );
    let mut sidebar = super::sidebar::Sidebar::default();
    sidebar.selected = task.id.clone();
    super::App {
        manager,
        store,
        task,
        cwd,
        transcript: TranscriptModel::new(),
        drawn: u64::MAX,
        blocks: Vec::new(),
        run_blocks: Vec::new(),
        palette: palette(),
        // Wide enough that the rail is shown, so a test that asserts on it is
        // asserting about content and not about a width threshold.
        width: 1400.0,
        draft: String::new(),
        working: false,
        model: "test-model".to_string(),
        screen: super::Screen::Chat,
        plan: None,
        interview: None,
        console: None,
        sidebar,
        rail: super::rail::Rail::default(),
        settings,
        menu: super::command::Menu::default(),
        pane: super::pane::Pane::Chat,
        attachments: Vec::new(),
        pending_notice: None,
        generation: 0,
    }
}

/// Switching chats is a *replacement*, not an overlay.
///
/// The failure this guards against is the quiet one: a subagent rail, a context
/// reading or a half-answered plan carried across a switch are another chat's
/// facts rendered under this chat's name, and nothing about the window looks
/// wrong while it happens. So every per-chat field is asserted cleared, and the
/// generation is asserted bumped, because that is what re-taps the feed.
#[tokio::test(flavor = "multi_thread")]
async fn switching_chats_replaces_every_per_chat_fact_and_re_taps_the_feed() {
    let workspace = tempfile::tempdir().expect("workspace");
    let _config_on_disk = OnDisk::write(&Config::default());
    let manager = Arc::new(TaskManager::with_registry(
        Arc::new(ConfigStore::new(Config::default())),
        Arc::new(RwLock::new(McpManager::empty())),
        None,
    ));
    let first = manager
        .create_task(workspace.path(), None, None)
        .expect("the first chat");
    let second = manager
        .create_task(workspace.path(), None, None)
        .expect("the second chat");
    let task = manager.get(&first).expect("live");
    let mut app = window(Arc::clone(&manager), task, workspace.path().to_path_buf());

    // Fill the first chat with everything that is per-chat.
    app.absorb(AgentEvent::TextDelta("first chat".to_string()));
    app.absorb(AgentEvent::ContextSize { tokens: 4_000 });
    app.absorb(AgentEvent::SubagentRunStarted {
        run: 1,
        bg: None,
        name: "scout".to_string(),
        task: "look around".to_string(),
    });
    app.absorb(AgentEvent::TodoUpdated(vec![
        crate::tools::todo::TodoItem {
            content: "one".to_string(),
            status: crate::tools::todo::TodoStatus::InProgress,
        },
    ]));
    app.attachments.push(PathBuf::from("/tmp/a.png"));
    app.refresh();
    assert!(!app.blocks.is_empty());
    assert_eq!(app.rail.subagents.runs.len(), 1);
    assert_eq!(app.rail.meter.context, Some(4_000));
    let generation = app.generation;

    let _ = app.adopt(super::Opened {
        id: second.clone(),
        cwd: workspace.path().to_path_buf(),
        model: "other-model".to_string(),
        entries: Vec::new(),
    });
    app.refresh();

    assert_eq!(app.task.id, second, "the window is on the other chat");
    assert_eq!(app.sidebar.selected, second);
    assert_eq!(app.model, "other-model");
    assert!(app.blocks.is_empty(), "the first chat's words are gone");
    assert!(app.rail.subagents.runs.is_empty(), "and its subagents");
    assert_eq!(app.rail.meter.context, None, "and its context reading");
    assert!(app.rail.todos.is_empty(), "and its todos");
    assert!(app.attachments.is_empty(), "and its staged files");
    assert!(app.pane.is_chat(), "and whatever pane was open");
    assert_eq!(
        app.generation,
        generation + 1,
        "the feed has to be rebuilt, or the window watches the chat it left"
    );

    manager.shutdown();
}

/// The whole window draws: sidebar, top bar, transcript, rail and composer, in
/// one tree, with no compositor. A panel that panics in `view` takes the window
/// with it, and the per-panel tests each build only their own subtree.
#[tokio::test(flavor = "multi_thread")]
async fn the_whole_window_draws_headlessly() -> Result<(), iced_test::Error> {
    let workspace = tempfile::tempdir().expect("workspace");
    let _config_on_disk = OnDisk::write(&Config::default());
    let manager = Arc::new(TaskManager::with_registry(
        Arc::new(ConfigStore::new(Config::default())),
        Arc::new(RwLock::new(McpManager::empty())),
        None,
    ));
    let id = manager
        .create_task(workspace.path(), None, None)
        .expect("the chat");
    let task = manager.get(&id).expect("live");
    let mut app = window(manager, task, workspace.path().to_path_buf());

    app.absorb(AgentEvent::TextDelta("a reply".to_string()));
    app.absorb(AgentEvent::ContextSize { tokens: 4_000 });
    app.rail.meter.window = Some(100_000);
    app.absorb(AgentEvent::TodoUpdated(vec![
        crate::tools::todo::TodoItem {
            content: "wire the rail".to_string(),
            status: crate::tools::todo::TodoStatus::InProgress,
        },
    ]));
    app.absorb(AgentEvent::SubagentRunStarted {
        run: 7,
        bg: None,
        name: "scout".to_string(),
        task: "look around".to_string(),
    });
    app.refresh();

    let mut ui = iced_test::simulator(super::view(&app));
    assert!(ui.find("New Chat").is_ok(), "the sidebar");
    assert!(ui.find("wire the rail").is_ok(), "the todo checklist");
    assert!(ui.find("scout").is_ok(), "the subagent rail");
    assert!(ui.find("test-model").is_ok(), "the composer's model chip");
    assert!(
        ui.find(crate::native::probe::contains("4K of 100K"))
            .is_ok(),
        "the context meter"
    );
    Ok(())
}

/// The `/` menu completes into the composer instead of sending, and a command
/// with arguments stops with a trailing space rather than running half-typed.
#[tokio::test(flavor = "multi_thread")]
async fn the_slash_menu_completes_before_it_runs() {
    let workspace = tempfile::tempdir().expect("workspace");
    let _config_on_disk = OnDisk::write(&Config::default());
    let manager = Arc::new(TaskManager::with_registry(
        Arc::new(ConfigStore::new(Config::default())),
        Arc::new(RwLock::new(McpManager::empty())),
        None,
    ));
    let id = manager
        .create_task(workspace.path(), None, None)
        .expect("the chat");
    let task = manager.get(&id).expect("live");
    let mut app = window(manager, task, workspace.path().to_path_buf());

    let _ = super::update(&mut app, super::Message::DraftChanged("/mod".to_string()));
    assert!(!app.menu.entries.is_empty(), "the menu opened");
    // `/model [tag]` takes an argument, so Enter completes and waits.
    let _ = super::update(&mut app, super::Message::Send);
    assert_eq!(app.draft, "/model ");
    assert!(
        app.menu.entries.is_empty(),
        "and the menu closed, because arguments have started"
    );

    // A command that takes none runs on the first Enter.
    let _ = super::update(&mut app, super::Message::DraftChanged("/diff".to_string()));
    let _ = super::update(&mut app, super::Message::Send);
    assert!(app.draft.is_empty(), "it ran rather than completing");
}

/// Escape backs out of whatever is on top, and onboarding is the one thing it
/// cannot back out of: there is nothing behind it to reach until a provider is
/// configured, and a chat with nothing to send a message to is worse than a
/// sheet that will not close.
#[tokio::test(flavor = "multi_thread")]
async fn escape_closes_the_sheet_unless_it_is_onboarding() {
    let workspace = tempfile::tempdir().expect("workspace");
    let _config_on_disk = OnDisk::write(&Config::default());
    let manager = Arc::new(TaskManager::with_registry(
        Arc::new(ConfigStore::new(Config::default())),
        Arc::new(RwLock::new(McpManager::empty())),
        None,
    ));
    let id = manager
        .create_task(workspace.path(), None, None)
        .expect("the chat");
    let task = manager.get(&id).expect("live");
    let mut app = window(manager, task, workspace.path().to_path_buf());

    // A default config has no providers, so the sheet is onboarding.
    assert!(app.settings.first_run());
    app.screen = super::Screen::Settings;
    let _ = super::update(&mut app, super::Message::Escape);
    assert!(
        matches!(app.screen, super::Screen::Settings),
        "onboarding does not dismiss"
    );

    // With one configured it is Settings, and it does.
    app.settings.view.first_run = false;
    let _ = super::update(&mut app, super::Message::Escape);
    assert!(matches!(app.screen, super::Screen::Chat));

    // And on the chat, Escape closes the pane instead.
    app.pane = super::pane::Pane::Image(PathBuf::from("/img/a.png"));
    let _ = super::update(&mut app, super::Message::Escape);
    assert!(app.pane.is_chat());
}

/// The window offers no way into the graph explorer.
///
/// This is the deferral from [`super::graph`] asserted rather than trusted:
/// the explorer is compiled and its own tests still run, but nothing in the
/// window routes to it. It replaces `the_mesh_screen_is_reachable_and_backs_out`,
/// and it is deliberately the same shape pointed the other way — whoever puts
/// the screen back will delete this and restore that one.
///
/// Worth stating plainly: a screen that is built and reachable from nothing is
/// exactly what the graph plugin already was for a whole release. That is a real
/// cost of holding it, and the reason it is written down here and in the
/// module rather than left for someone to rediscover.
#[test]
fn the_window_has_no_route_into_the_graph_explorer() {
    // Comments are stripped first: both files *discuss* the explorer at
    // length, and the point of the deferral is that the note explaining it
    // survives. Only what the compiler reads is scanned.
    let code = |source: &str| {
        source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    for (file, source) in [
        ("native/mod.rs", include_str!("mod.rs")),
        ("native/sidebar.rs", include_str!("sidebar.rs")),
    ] {
        let code = code(source);
        for seam in ["Screen::Mesh", "OpenMesh", "Message::Graph", "explorer"] {
            assert!(
                !code.contains(seam),
                "{file} still names `{seam}` in code: the explorer is deferred, so the \
                 window must not be able to reach it"
            );
        }
    }
}

/// The branch chip checks a branch out, and reports git's refusal when git
/// refuses.
///
/// `gui::git::checkout` was written for the browser GUI and, until this,
/// reused by nothing — a function with no caller is a function nobody notices
/// has stopped working. Both halves are asserted here because the interesting
/// one is the failure: a checkout that would overwrite an uncommitted change
/// must surface git's own words rather than being forced through, and a window
/// that silently swallowed the error would look identical to one that switched.
#[tokio::test(flavor = "multi_thread")]
async fn the_branch_chip_checks_out_and_reports_a_refusal() {
    let workspace = tempfile::tempdir().expect("workspace");
    let root = workspace.path();
    let git = |args: &[&str]| {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("run git")
            .status
            .success();
        assert!(ok, "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(root.join("a.txt"), "one\n").expect("write");
    git(&["add", "a.txt"]);
    git(&["commit", "-qm", "one"]);
    // The two branches have to *differ* in `a.txt`, or git carries an
    // uncommitted change across the switch and there is no refusal to assert.
    git(&["checkout", "-q", "-b", "side"]);
    std::fs::write(root.join("a.txt"), "side\n").expect("write");
    git(&["commit", "-qam", "side"]);
    git(&["checkout", "-q", "main"]);

    let _config_on_disk = OnDisk::write(&Config::default());
    let manager = Arc::new(TaskManager::with_registry(
        Arc::new(ConfigStore::new(Config::default())),
        Arc::new(RwLock::new(McpManager::empty())),
        None,
    ));
    let id = manager.create_task(root, None, None).expect("the chat");
    let task = manager.get(&id).expect("live");
    let mut app = window(Arc::clone(&manager), task, root.to_path_buf());

    // Opening the chip lists the branches, and the list is what the rail draws.
    let _ = super::update(
        &mut app,
        super::Message::Rail(super::rail::Message::ToggleBranches),
    );
    assert!(app.rail.branches_open, "the chip opened");
    let listed = crate::gui::git::branches(root).await.expect("branches");
    app.rail.branches_loaded(listed.branches);
    assert!(
        app.rail.branches.iter().any(|name| name == "side"),
        "the other branch is offered: {:?}",
        app.rail.branches
    );

    // Clicking one switches, and the rail closes behind it.
    let switched = crate::gui::git::checkout(root, "side", false)
        .await
        .expect("checkout");
    let _ = super::update(
        &mut app,
        super::Message::CheckedOut(Box::new(Ok(switched.clone()))),
    );
    assert_eq!(switched, "side");
    assert!(!app.rail.branches_open, "and the list closed");
    assert!(
        app.transcript
            .items()
            .iter()
            .any(|item| matches!(item, TranscriptItem::Notice(text) if text.contains("side"))),
        "the switch is stated in the transcript"
    );

    // A dirty tree is git's to refuse, and the refusal is the user's to read.
    std::fs::write(root.join("a.txt"), "two\n").expect("write");
    let refused = crate::gui::git::checkout(root, "main", false)
        .await
        .expect_err("git refuses to overwrite an uncommitted change");
    let _ = super::update(
        &mut app,
        super::Message::CheckedOut(Box::new(Err(format!("{refused:#}")))),
    );
    assert!(
        app.transcript
            .items()
            .iter()
            .any(|item| matches!(item, TranscriptItem::Notice(text) if text.contains("a.txt"))),
        "git's own words name the file, and they reach the window"
    );

    manager.shutdown();
}

/// The sidebar's footer changes where the *next* new chat opens, and changes
/// nothing about the chat on screen.
///
/// That asymmetry is the whole behaviour: a session's directory is fixed when
/// it is created, so a control that appeared to move the open chat would be
/// claiming something the session store does not support.
#[tokio::test(flavor = "multi_thread")]
async fn the_footer_moves_where_a_new_chat_opens_and_nothing_else() {
    let here = tempfile::tempdir().expect("here");
    let there = tempfile::tempdir().expect("there");
    let _config_on_disk = OnDisk::write(&Config::default());
    let manager = Arc::new(TaskManager::with_registry(
        Arc::new(ConfigStore::new(Config::default())),
        Arc::new(RwLock::new(McpManager::empty())),
        None,
    ));
    let id = manager
        .create_task(here.path(), None, None)
        .expect("the chat");
    let task = manager.get(&id).expect("live");
    let mut app = window(Arc::clone(&manager), task, here.path().to_path_buf());

    let _ = super::update(
        &mut app,
        super::Message::Sidebar(super::sidebar::Message::UseWorkspace(
            there.path().display().to_string(),
        )),
    );
    assert_eq!(app.cwd, there.path(), "the next new chat opens there");
    assert_eq!(
        app.task.cwd,
        here.path(),
        "and the chat on screen is still where it was created"
    );

    // `new_chat` creates the session and then *opens* it, and opening is
    // asynchronous — the window adopts it when the read off disk lands. So the
    // assertion is on the session that was created, not on the window's own
    // pointer, which has not moved yet.
    let before: std::collections::HashSet<String> = manager.registry_states().into_keys().collect();
    let _ = app.new_chat();
    let created = manager
        .registry_states()
        .into_keys()
        .find(|id| !before.contains(id))
        .expect("new_chat created a session");
    let opened = manager.get(&created).expect("the new chat is live");
    assert_eq!(opened.cwd, there.path(), "and the new one landed there");

    manager.shutdown();
}

/// The diff surfaces use the diff tokens, not the semantic ones.
///
/// `Token::Error` and `Token::DiffDel` are different colours on purpose, and
/// the shipped `minimal` theme is where it shows: it is monochrome, sets
/// `error = "white"`, and then defines `diff.del = "red"` under a comment
/// saying the diff is the one place a hue carries the meaning. Reading `Error`
/// in the diff pane took the white, so a deleted line rendered with a neutral
/// wash that reads as *highlighted* rather than removed.
///
/// A source scan, because what failed was which constant was named — there is
/// no rendered pixel a headless test can sample, and the palette lookup is
/// correct for whatever token it is handed.
#[test]
fn the_windows_diff_uses_the_diff_tokens_and_not_the_semantic_ones() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/native");
    for (file, what) in [
        ("pane.rs", "the diff pane"),
        ("rail.rs", "the rail's diffstat"),
    ] {
        let source = std::fs::read_to_string(root.join(file))
            .unwrap_or_else(|_| panic!("src/native/{file}"));
        // Only the diff-drawing region of each file: `Token::Error` is a
        // legitimate choice elsewhere in both (a failed tool, a notice).
        let start = source
            .find("LineKind::Add")
            .or_else(|| source.find("+{additions}"))
            .unwrap_or_else(|| panic!("{what} draws additions somewhere in {file}"));
        let region = &source[start..(start + 600).min(source.len())];

        for token in ["Token::Success", "Token::Error"] {
            assert!(
                !region.contains(token),
                "{what} ({file}) reaches for {token}; the diff has its own \
                 tokens and `minimal` deliberately paints {token} white"
            );
        }
        assert!(
            region.contains("Token::DiffAdd") && region.contains("Token::DiffDel"),
            "{what} ({file}) should name both diff tokens"
        );
    }
}
