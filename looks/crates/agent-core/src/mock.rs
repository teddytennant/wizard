//! Scripted backend for screenshots and UI tests: no process, no model.
//!
//! Selected with `OPENW_BACKEND=mock` / `OPENC_BACKEND=mock`. A prompt picks a
//! scenario by keyword, so a test can ask for exactly the screen it wants:
//!
//! - `tools`: bash, read, grep, edit with a diff, write, todos, then prose
//! - `long`: a long markdown answer with headings, lists, a table and code
//! - `think`: reasoning, then a short answer
//! - `error`: a failing tool, then a warning notice
//! - `slow`: streams for ~20 s so busy states and interrupts can be shot
//! - `sub`: a subagent run with nested tool calls
//! - `perm`: an edit that waits for `Request::Decide` (`perm bash` asks to run a command)
//! - `busy`: a bash call that stays running with output, for ~30 s
//! - `gallery`: one finished call per wizard tool (`gallery live` leaves the last three running)
//! - `rich`: output variety: ANSI colour, a failing exit code, a very long line, no output,
//!   binary output, a long read, search results, fetch, an MCP call, a denied edit
//! - `parallel`: three calls of different kinds running at once, finishing one by one
//! - `agents`: two subagents running side by side with nested calls, one finishing first
//! - `perm fetch|mcp|write|read|agent`: permission panels for other tool kinds
//! - `ask me`: an `AskUserQuestion` (one single select, one multi select) that waits for
//!   `Request::Answer`
//! - `exit plan`: an `ExitPlanMode` approval that waits for `Request::Decide`
//! - `edits`: three edits across two files, for the diff viewer
//! - `[[echo]] text`: answers with `text` verbatim
//! - anything else: a short answer
//!
//! `RewindPreview` and `Rewind` answer from the prompts seen so far.
//!
//! `/status`, `/help` and friends answer with a Notice. `Cancel` ends a running
//! turn with `Cancelled`.

use std::time::Duration;

use tokio::sync::mpsc;

use crate::*;

mod gallery;
pub use gallery::gallery_calls;

pub struct MockBackend;

/// Knobs for frontends that want the mock to look more like a live backend. The default is what
/// `Backend::spawn` uses, so existing callers see no change.
#[derive(Clone, Copy, Debug, Default)]
pub struct MockOpts {
    /// The `tools` scenario also emits the todo tool call that wizard sends next to
    /// `Event::Todos`, so the transcript gets its `# Todos` block.
    pub todo_tool: bool,
    /// Start with no saved sessions and no connected provider, like a fresh data dir;
    /// `ListSessions` then answers with the sessions this run created (one once a prompt has
    /// been sent), and the first prompt brings the models.
    pub no_sessions: bool,
}

pub fn commands() -> Vec<SlashCommand> {
    let c = |n: &str, d: &str, h: &str| SlashCommand {
        name: n.into(),
        description: d.into(),
        input_hint: h.into(),
    };
    vec![
        c("model", "switch model", "<provider>/<model>"),
        c("mode", "switch mode", "<genie|sovereign>"),
        c("effort", "set reasoning effort", "<low|medium|high>"),
        c("plan", "toggle plan mode", ""),
        c("compact", "summarize older history", ""),
        c("diff", "show the working tree diff", ""),
        c("todos", "show the todo list", ""),
        c("status", "show status", ""),
        c("cost", "show cost", ""),
        c("agents", "list subagents", ""),
        c("help", "show help", ""),
    ]
}

/// The saved sessions the mock lists: two in the directory it was started in, one elsewhere.
fn sessions(cwd: &std::path::Path) -> Vec<SessionInfo> {
    // Fixed times keep snapshots stable; `AGENT_CORE_MOCK_RECENT` counts back from now instead,
    // for screens that want `11s ago`.
    let recent = std::env::var_os("AGENT_CORE_MOCK_RECENT").is_some();
    let base = if recent {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1_790_000_000, |d| d.as_secs() as i64)
    } else {
        1_790_000_000
    };
    let s = |id: &str, t: &str, dir: &str, ago: i64, recent_ago: i64| SessionInfo {
        id: id.into(),
        title: t.into(),
        cwd: dir.into(),
        updated: base - if recent { recent_ago } else { ago },
    };
    let here = cwd.display().to_string();
    vec![
        s("a", "Fix the flaky parser test", &here, 600, 11),
        s("b", "Add a --json flag", &here, 90_000, 24),
        s("c", "Explain the build graph", "/tmp/demo", 400_000, 37),
    ]
}

pub fn config() -> Config {
    let m = |p: &str, n: &str| ModelOption {
        id: format!("{p}/{n}"),
        name: n.into(),
        provider: p.into(),
    };
    Config {
        model: "xai-oauth/grok-4.6".into(),
        models: vec![
            m("xai-oauth", "grok-4.6"),
            m("xai-oauth", "grok-4.6-fast"),
            m("chatgpt", "gpt-5.6-sol"),
            m("openrouter", "anthropic/claude-sonnet-5"),
        ],
        effort: "default".into(),
        efforts: ["default", "low", "medium", "high", "xhigh"]
            .map(String::from)
            .to_vec(),
        mode: "genie".into(),
        modes: vec!["genie".into(), "sovereign".into()],
        cwd: std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        backend: "mock 0.0".into(),
    }
}

fn tool(
    id: &str,
    name: &str,
    kind: ToolKind,
    title: &str,
    input: serde_json::Value,
    status: ToolStatus,
    output: Option<&str>,
) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        kind,
        title: title.into(),
        input,
        status,
        output: output.map(Into::into),
        diff: None,
        parent_id: None,
    }
}

const LONG: &str = "# Plan\n\nHere is what I found in **`src/`**, with a few *notes*.\n\n## Findings\n\n1. `main.rs` prints a greeting\n2. There is no test directory\n   - add `tests/smoke.rs`\n   - wire it into CI\n3. [Docs](https://example.com/docs) are missing\n\n> Keep the change small; one commit per fix.\n\n| file | lines | status |\n|------|------:|--------|\n| src/main.rs | 3 | ok |\n| src/lib.rs | 120 | needs tests |\n\n```rust\nfn main() {\n    let name = std::env::args().nth(1).unwrap_or_else(|| \"world\".into());\n    println!(\"hello, {name}\");\n}\n```\n\n---\n\nThat is all.";

impl Backend for MockBackend {
    fn spawn(cwd: std::path::PathBuf, resume: Option<String>) -> anyhow::Result<BackendHandle> {
        Self::spawn_with(cwd, resume, MockOpts::default())
    }
}

impl MockBackend {
    pub fn spawn_with(
        cwd: std::path::PathBuf,
        _resume: Option<String>,
        opts: MockOpts,
    ) -> anyhow::Result<BackendHandle> {
        let (tx, mut req_rx) = mpsc::unbounded_channel::<Request>();
        let (ev_tx, rx) = mpsc::unbounded_channel::<Event>();
        tokio::spawn(async move {
            // Model, effort and mode changes stick, so a frontend can be driven through them.
            let mut cfg = config();
            // A fresh data dir has no provider connected yet; the first prompt connects one.
            let mut unconnected = opts.no_sessions.then(|| std::mem::take(&mut cfg.models));
            let _ = ev_tx.send(Event::Ready {
                session_id: "mock-1".into(),
                config: cfg.clone(),
            });
            let _ = ev_tx.send(Event::Commands(commands()));
            let mut running: Option<tokio::task::JoinHandle<()>> = None;
            let (dtx, drx) = mpsc::unbounded_channel::<(String, bool, String)>();
            let mut seen: Vec<String> = Vec::new();
            let drx = std::sync::Arc::new(tokio::sync::Mutex::new(drx));
            while let Some(req) = req_rx.recv().await {
                match req {
                    Request::Prompt(text) | Request::PromptWith { text, .. } => {
                        if text.starts_with('/') {
                            let name = text
                                .trim_start_matches('/')
                                .split_whitespace()
                                .next()
                                .unwrap_or("");
                            let _ = ev_tx.send(Event::Notice {
                                level: NoticeLevel::Info,
                                text: format!("/{name}: mock output"),
                            });
                            continue;
                        }
                        seen.push(text.clone());
                        if let Some(models) = unconnected.take() {
                            cfg.models = models;
                            let _ = ev_tx.send(Event::ConfigChanged(cfg.clone()));
                        }
                        let ev = ev_tx.clone();
                        let d = drx.clone();
                        running = Some(tokio::spawn(async move {
                            scenario(&text, ev, d, opts.todo_tool).await
                        }));
                    }
                    Request::Cancel => {
                        if let Some(h) = running.take() {
                            h.abort();
                            let _ = ev_tx.send(Event::TurnEnd(StopReason::Cancelled));
                        }
                    }
                    Request::Decide {
                        id, allow, note, ..
                    } => {
                        let _ = dtx.send((id, allow, note));
                    }
                    Request::Answer { id, answers } => {
                        let joined = answers
                            .iter()
                            .map(|(q, a)| format!("{q}\t{a}"))
                            .collect::<Vec<_>>()
                            .join("\n");
                        let _ = dtx.send((id, true, joined));
                    }
                    Request::RewindPreview { turn } => {
                        let ok = turn < seen.len();
                        let _ = ev_tx.send(Event::RewindPreview(RewindPreview {
                            turn,
                            can_rewind_files: ok && turn > 0,
                            files: if ok && turn > 0 {
                                vec!["src/main.rs".into(), "notes.txt".into()]
                            } else {
                                vec![]
                            },
                            insertions: 4,
                            deletions: 1,
                            can_rewind_conversation: ok,
                            note: if turn == 0 {
                                "files: no snapshot for the first message".into()
                            } else {
                                String::new()
                            },
                        }));
                    }
                    Request::Rewind {
                        turn, conversation, ..
                    } => {
                        if let Some(h) = running.take() {
                            h.abort();
                        }
                        if conversation {
                            let items = seen
                                .iter()
                                .take(turn)
                                .flat_map(|t| {
                                    [
                                        HistoryItem::User(t.clone()),
                                        HistoryItem::Assistant("Done.".into()),
                                    ]
                                })
                                .collect();
                            seen.truncate(turn);
                            let _ = ev_tx.send(Event::History {
                                session_id: "mock-rw".into(),
                                items,
                            });
                        } else {
                            let _ = ev_tx.send(Event::Notice {
                                level: NoticeLevel::Info,
                                text: "files put back".into(),
                            });
                        }
                    }
                    Request::Shutdown => break,
                    Request::NewSession => {
                        let _ = ev_tx.send(Event::History {
                            session_id: "mock-2".into(),
                            items: vec![],
                        });
                        // A real backend confirms every session switch with `Ready`.
                        let _ = ev_tx.send(Event::Ready {
                            session_id: "mock-2".into(),
                            config: cfg.clone(),
                        });
                    }
                    Request::LoadSession(id) => {
                        let title = sessions(&cwd)
                            .into_iter()
                            .find(|s| s.id == id)
                            .map_or_else(|| format!("session {id}"), |s| s.title);
                        let _ = ev_tx.send(Event::History {
                            session_id: id.clone(),
                            items: vec![
                                HistoryItem::User(title),
                                HistoryItem::Assistant("Picked up where we left off.".into()),
                            ],
                        });
                        let _ = ev_tx.send(Event::Ready {
                            session_id: id,
                            config: cfg.clone(),
                        });
                    }
                    Request::ListSessions if opts.no_sessions => {
                        let made = if seen.is_empty() {
                            Vec::new()
                        } else {
                            vec![SessionInfo {
                                id: "mock-1".into(),
                                title: "Mock title".into(),
                                cwd: "/tmp/demo".into(),
                                // today, so the dialog files it under `Today` like a live run
                                updated: std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map_or(0, |d| d.as_secs() as i64),
                            }]
                        };
                        let _ = ev_tx.send(Event::Sessions(made));
                    }
                    Request::ListSessions => {
                        let _ = ev_tx.send(Event::Sessions(sessions(&cwd)));
                    }
                    Request::SetModel(m) => {
                        cfg.model = m;
                        let _ = ev_tx.send(Event::ConfigChanged(cfg.clone()));
                    }
                    Request::SetEffort(e) => {
                        cfg.effort = e;
                        let _ = ev_tx.send(Event::ConfigChanged(cfg.clone()));
                    }
                    Request::SetMode(m) => {
                        cfg.mode = m;
                        let _ = ev_tx.send(Event::ConfigChanged(cfg.clone()));
                    }
                    _ => {}
                }
            }
        });
        Ok(BackendHandle { tx, rx })
    }
}

async fn pause(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

async fn stream(ev: &mpsc::UnboundedSender<Event>, text: &str) {
    let mut chunk = String::new();
    for (i, w) in text.split_inclusive(' ').enumerate() {
        chunk.push_str(w);
        if i % 3 == 2 {
            let _ = ev.send(Event::TextDelta(std::mem::take(&mut chunk)));
            pause(15).await;
        }
    }
    if !chunk.is_empty() {
        let _ = ev.send(Event::TextDelta(chunk));
    }
}

type Decisions =
    std::sync::Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<(String, bool, String)>>>;

async fn scenario(
    prompt: &str,
    ev: mpsc::UnboundedSender<Event>,
    decisions: Decisions,
    todo_tool: bool,
) {
    let p = prompt.to_lowercase();
    let _ = ev.send(Event::TurnStart);
    if let Some(rest) = prompt.strip_prefix("[[echo]]") {
        // answer with the rest of the prompt verbatim, for fidelity runs on arbitrary markdown
        stream(&ev, rest.trim_start_matches([' ', '\n'])).await;
        let _ = ev.send(Event::Usage(Usage {
            input_tokens: 11_200,
            output_tokens: 640,
            cached_tokens: 8_000,
            context_tokens: 11_840,
            context_window: 200_000,
            cost_usd: Some(0.0412),
        }));
        let _ = ev.send(Event::TurnEnd(StopReason::EndTurn));
        return;
    }
    if p.contains("gallery") {
        let calls = gallery_calls();
        let live = p.contains("live");
        for (i, c) in calls.iter().enumerate() {
            if live && i + 3 >= calls.len() {
                let _ = ev.send(Event::Tool(ToolCall {
                    status: ToolStatus::Running,
                    output: None,
                    ..c.clone()
                }));
            } else {
                let _ = ev.send(Event::Tool(c.clone()));
            }
        }
        if live {
            pause(30_000).await;
        }
        stream(&ev, "That is every tool once.").await;
    } else if p.contains("think") {
        for w in [
            "Let me look at the shape of the problem first. ",
            "The parser reads a token and then peeks, ",
            "so the off by one is in the lookahead.",
        ] {
            let _ = ev.send(Event::ThoughtDelta(w.into()));
            pause(120).await;
        }
        stream(
            &ev,
            "The bug is in the lookahead: it consumes one token too many.",
        )
        .await;
    } else if p.contains("long") {
        stream(&ev, LONG).await;
    } else if p.contains("slow") {
        for i in 0..40 {
            stream(&ev, &format!("Working through item {i} of 40. ")).await;
            pause(500).await;
        }
    } else if p.contains("error") {
        let c = tool(
            "e1",
            "execute",
            ToolKind::Execute,
            "cargo test",
            serde_json::json!({"command": "cargo test"}),
            ToolStatus::Running,
            None,
        );
        let _ = ev.send(Event::Tool(c.clone()));
        pause(400).await;
        let _ = ev.send(Event::Tool(ToolCall {
            status: ToolStatus::Failed,
            output: Some(
                "error[E0425]: cannot find value `x` in this scope\n --> src/main.rs:3:5".into(),
            ),
            ..c
        }));
        let _ = ev.send(Event::Notice {
            level: NoticeLevel::Warn,
            text: "stream retrying (attempt 2)".into(),
        });
        stream(&ev, "The build failed on an undefined `x`.").await;
    } else if p.contains("perm") {
        stream(&ev, "I'll update the greeting. ").await;
        let (req, call) = if p.contains("fetch") {
            let input = serde_json::json!({"url": "https://docs.rs/ratatui/latest/ratatui/", "prompt": "List the widgets"});
            (
                PermissionRequest {
                    id: "perm-1".into(),
                    tool: "WebFetch".into(),
                    kind: ToolKind::Fetch,
                    title: "https://docs.rs/ratatui/latest/ratatui/".into(),
                    input: input.clone(),
                    diff: None,
                    rule: "docs.rs".into(),
                },
                tool(
                    "pf",
                    "WebFetch",
                    ToolKind::Fetch,
                    "https://docs.rs/ratatui/latest/ratatui/",
                    input,
                    ToolStatus::Running,
                    None,
                ),
            )
        } else if p.contains("mcp") {
            let input = serde_json::json!({"selector": "button#submit", "element": "Submit button", "timeout": 5000});
            (
                PermissionRequest {
                    id: "perm-1".into(),
                    tool: "mcp__brave__browser_click".into(),
                    kind: ToolKind::Other,
                    title: "brave:browser_click".into(),
                    input: input.clone(),
                    diff: None,
                    rule: "mcp__brave".into(),
                },
                tool(
                    "pm",
                    "mcp__brave__browser_click",
                    ToolKind::Other,
                    "brave:browser_click",
                    input,
                    ToolStatus::Running,
                    None,
                ),
            )
        } else if p.contains("write") {
            let content: String = (1..=30)
                .map(|i| format!("line {i} of the new file\n"))
                .collect();
            let diff = FileDiff {
                path: "docs/NEW.md".into(),
                old: None,
                new: content,
            };
            let mut c = tool(
                "pw",
                "Write",
                ToolKind::Edit,
                "docs/NEW.md",
                serde_json::json!({"file_path": "docs/NEW.md"}),
                ToolStatus::Running,
                None,
            );
            c.diff = Some(diff.clone());
            (
                PermissionRequest {
                    id: "perm-1".into(),
                    tool: "Write".into(),
                    kind: ToolKind::Edit,
                    title: "docs/NEW.md".into(),
                    input: serde_json::json!({"file_path": "docs/NEW.md"}),
                    diff: Some(diff),
                    rule: "edits this session".into(),
                },
                c,
            )
        } else if p.contains("read") {
            let input = serde_json::json!({"file_path": "/etc/hosts"});
            (
                PermissionRequest {
                    id: "perm-1".into(),
                    tool: "Read".into(),
                    kind: ToolKind::Read,
                    title: "/etc/hosts".into(),
                    input: input.clone(),
                    diff: None,
                    rule: "/etc/**".into(),
                },
                tool(
                    "pr",
                    "Read",
                    ToolKind::Read,
                    "/etc/hosts",
                    input,
                    ToolStatus::Running,
                    None,
                ),
            )
        } else if p.contains("agent") {
            let input = serde_json::json!({"description": "survey callers of wrap()", "subagent_type": "Explore", "prompt": "Find every caller of render::wrap and say whether it passes a width."});
            (
                PermissionRequest {
                    id: "perm-1".into(),
                    tool: "Agent".into(),
                    kind: ToolKind::Think,
                    title: "survey callers of wrap()".into(),
                    input: input.clone(),
                    diff: None,
                    rule: String::new(),
                },
                tool(
                    "pg",
                    "Agent",
                    ToolKind::Think,
                    "survey callers of wrap()",
                    input,
                    ToolStatus::Running,
                    None,
                ),
            )
        } else if p.contains("bash") {
            let input = serde_json::json!({"command": "cargo test --workspace", "description": "Run the whole test suite"});
            (
                PermissionRequest {
                    id: "perm-1".into(),
                    tool: "Bash".into(),
                    kind: ToolKind::Execute,
                    title: "cargo test --workspace".into(),
                    input: input.clone(),
                    diff: None,
                    rule: "cargo test *".into(),
                },
                tool(
                    "pb",
                    "Bash",
                    ToolKind::Execute,
                    "cargo test --workspace",
                    input,
                    ToolStatus::Running,
                    None,
                ),
            )
        } else {
            let diff = FileDiff {
                path: "src/main.rs".into(),
                old: Some("fn main() {\n    println!(\"hi\");\n}\n".into()),
                new:
                    "fn main() {\n    let name = \"world\";\n    println!(\"hello, {name}\");\n}\n"
                        .into(),
            };
            let mut c = tool(
                "pe",
                "Edit",
                ToolKind::Edit,
                "src/main.rs",
                serde_json::json!({"file_path": "src/main.rs"}),
                ToolStatus::Running,
                None,
            );
            c.diff = Some(diff.clone());
            (
                PermissionRequest {
                    id: "perm-1".into(),
                    tool: "Edit".into(),
                    kind: ToolKind::Edit,
                    title: "src/main.rs".into(),
                    input: serde_json::json!({"file_path": "src/main.rs"}),
                    diff: Some(diff),
                    rule: "src/**".into(),
                },
                c,
            )
        };
        let _ = ev.send(Event::Tool(call.clone()));
        let _ = ev.send(Event::Permission(req));
        let decided = decisions.lock().await.recv().await;
        match decided {
            Some((_, true, _)) => {
                let _ = ev.send(Event::Tool(ToolCall {
                    status: ToolStatus::Completed,
                    output: Some("ok".into()),
                    ..call
                }));
                stream(&ev, "Done, the change is in.").await;
            }
            Some((_, false, note)) => {
                let _ = ev.send(Event::Tool(ToolCall {
                    status: ToolStatus::Failed,
                    output: Some("The user denied this tool call.".into()),
                    ..call
                }));
                stream(&ev, &format!("Understood, I left it alone. {note}")).await;
            }
            None => {}
        }
    } else if p.contains("rich") {
        rich(&ev).await;
    } else if p.contains("parallel") {
        let a = tool(
            "pa",
            "Bash",
            ToolKind::Execute,
            "cargo build --release",
            serde_json::json!({"command": "cargo build --release"}),
            ToolStatus::Running,
            Some("   Compiling serde v1.0.229\n   Compiling tokio v1.53.2"),
        );
        let b = tool(
            "pb",
            "WebFetch",
            ToolKind::Fetch,
            "https://doc.rust-lang.org/std/",
            serde_json::json!({"url": "https://doc.rust-lang.org/std/", "prompt": "summarize"}),
            ToolStatus::Running,
            None,
        );
        let c = tool(
            "pc",
            "Bash",
            ToolKind::Execute,
            "pytest -x tests/",
            serde_json::json!({"command": "pytest -x tests/"}),
            ToolStatus::Running,
            Some("collected 212 items\n\ntests/test_a.py ....."),
        );
        for t in [&a, &b, &c] {
            let _ = ev.send(Event::Tool(t.clone()));
        }
        pause(1800).await;
        let _ = ev.send(Event::Tool(ToolCall {
            status: ToolStatus::Completed,
            output: Some("The std crate docs describe the standard library.".into()),
            ..b
        }));
        pause(1800).await;
        let _ = ev.send(Event::Tool(ToolCall {
            status: ToolStatus::Completed,
            output: Some("212 passed in 3.1s".into()),
            ..c
        }));
        pause(10_000).await;
        let _ = ev.send(Event::Tool(ToolCall {
            status: ToolStatus::Completed,
            output: Some("    Finished release [optimized] target(s) in 41s".into()),
            ..a
        }));
        stream(&ev, "All three finished.").await;
    } else if p.contains("agents") {
        agents(&ev).await;
    } else if p.contains("edits") {
        let src = "use std::io;\n\nfn greet(name: &str) {\n    println!(\"hi {name}\");\n}\n\nfn main() {\n    let name = \"world\";\n    greet(name);\n    let n = 3;\n    for i in 0..n {\n        println!(\"{i}\");\n    }\n    let _ = io::stdout();\n}\n";
        let edits: [(&str, &str, &str, &str); 3] = [
            (
                "e1",
                "src/main.rs",
                "    println!(\"hi {name}\");\n",
                "    println!(\"hello, {name}\");\n",
            ),
            (
                "e2",
                "src/main.rs",
                "    let n = 3;\n    for i in 0..n {\n        println!(\"{i}\");\n    }\n",
                "    for i in 0..5 {\n        println!(\"{i}\");\n    }\n",
            ),
            (
                "e3",
                "src/lib.rs",
                "pub fn greet() {}\n",
                "pub fn greet(name: &str) -> String {\n    format!(\"hello, {name}\")\n}\n",
            ),
        ];
        let _ = src;
        for (id, path, old, new) in edits {
            let mut e = tool(
                id,
                "Edit",
                ToolKind::Edit,
                path,
                serde_json::json!({"file_path": path}),
                ToolStatus::Completed,
                Some("The file has been updated."),
            );
            e.diff = Some(FileDiff {
                path: path.into(),
                old: Some(old.into()),
                new: new.into(),
            });
            let _ = ev.send(Event::Tool(e));
            pause(100).await;
        }
        let mut w = tool(
            "e4",
            "Write",
            ToolKind::Edit,
            "docs/NOTES.md",
            serde_json::json!({"file_path": "docs/NOTES.md"}),
            ToolStatus::Completed,
            Some("File created successfully"),
        );
        w.diff = Some(FileDiff {
            path: "docs/NOTES.md".into(),
            old: None,
            new: "# Notes\n\n- greeting takes a name\n- loop runs five times\n".into(),
        });
        let _ = ev.send(Event::Tool(w));
        stream(
            &ev,
            "Changed the greeting, the loop count and the library signature.",
        )
        .await;
    } else if p.contains("ask me") {
        let input = serde_json::json!({"questions": [
            {"question": "Which language should the tool be written in?", "header": "Language", "multiSelect": false,
             "options": [{"label": "Rust", "description": "Fast, single binary, slower to write"}, {"label": "Go", "description": "Fast to write, easy to ship"}, {"label": "Python", "description": "Quickest to prototype"}]},
            {"question": "Which extras do you want?", "header": "Extras", "multiSelect": true,
             "options": [{"label": "Tests", "description": "Unit and integration tests"}, {"label": "Docs", "description": "README and doc comments"}, {"label": "CI", "description": "GitHub Actions workflow"}]}
        ]});
        let call = tool(
            "ask1",
            "AskUserQuestion",
            ToolKind::Other,
            "Which language should the tool be written in? (+1 more)",
            input.clone(),
            ToolStatus::Running,
            None,
        );
        let _ = ev.send(Event::Tool(call.clone()));
        let _ = ev.send(Event::Permission(PermissionRequest {
            id: "ask-1".into(),
            tool: "AskUserQuestion".into(),
            kind: ToolKind::Other,
            title: call.title.clone(),
            input,
            diff: None,
            rule: String::new(),
        }));
        match decisions.lock().await.recv().await {
            Some((_, true, answers)) => {
                let text = answers
                    .lines()
                    .filter_map(|l| l.split_once('\t'))
                    .map(|(q, a)| format!("\"{q}\"=\"{a}\""))
                    .collect::<Vec<_>>()
                    .join(", ");
                let _ = ev.send(Event::Tool(ToolCall { status: ToolStatus::Completed, output: Some(format!("Your questions have been answered: {text}. You can now continue with these answers in mind.")), ..call }));
                stream(&ev, "Got it, I'll go with those.").await;
            }
            Some((_, false, note)) => {
                let _ = ev.send(Event::Tool(ToolCall {
                    status: ToolStatus::Failed,
                    output: Some(if note.is_empty() {
                        "The user denied this tool call.".into()
                    } else {
                        format!("The user denied this tool call and said: {note}")
                    }),
                    ..call
                }));
                stream(&ev, "No problem, I'll pick for you.").await;
            }
            None => {}
        }
    } else if p.contains("exit plan") {
        let plan = "## Add a `--width` flag\n\n1. Parse `--width N` in `main.rs`, default 80\n2. Thread the value into `render::wrap`\n3. Fix `test_wrap_cjk`, which assumed 80\n\n```rust\nfn wrap(line: &str, width: usize) -> Vec<String>\n```\n\nRisk: callers of `wrap` outside `render` keep the old behaviour.";
        let input = serde_json::json!({"plan": plan, "planFilePath": "/home/me/.claude/plans/add-width.md"});
        let call = tool(
            "plan1",
            "ExitPlanMode",
            ToolKind::Other,
            "Add a --width flag",
            input.clone(),
            ToolStatus::Running,
            None,
        );
        let _ = ev.send(Event::Tool(call.clone()));
        let _ = ev.send(Event::Permission(PermissionRequest {
            id: "plan-1".into(),
            tool: "ExitPlanMode".into(),
            kind: ToolKind::Other,
            title: String::new(),
            input,
            diff: None,
            rule: String::new(),
        }));
        match decisions.lock().await.recv().await {
            Some((_, true, _)) => {
                let _ = ev.send(Event::Tool(ToolCall {
                    status: ToolStatus::Completed,
                    output: Some("User has approved your plan. You can now start coding.".into()),
                    ..call
                }));
                stream(&ev, "Starting with the parser.").await;
            }
            Some((_, false, note)) => {
                let _ = ev.send(Event::Tool(ToolCall {
                    status: ToolStatus::Failed,
                    output: Some(if note.is_empty() {
                        "The user denied this tool call.".into()
                    } else {
                        format!("The user denied this tool call and said: {note}")
                    }),
                    ..call
                }));
                stream(&ev, "Back to planning.").await;
            }
            None => {}
        }
    } else if p.contains("busy") {
        let b = tool("busy1", "execute", ToolKind::Execute, "cargo test --workspace", serde_json::json!({"command": "cargo test --workspace"}), ToolStatus::Running, Some("test layout::wraps_cjk ... ok\ntest layout::long_token ... ok\ntest layout::tabs ... ok\ntest layout::resize ... ok"));
        let _ = ev.send(Event::Tool(b));
        pause(30_000).await;
    } else if p.contains("sub") {
        let parent = tool(
            "t1",
            "task",
            ToolKind::Other,
            "explore: find the parser",
            serde_json::json!({"description": "find the parser"}),
            ToolStatus::Running,
            None,
        );
        let _ = ev.send(Event::Tool(parent.clone()));
        for (i, (n, k, t)) in [
            ("search_files", ToolKind::Search, "parse"),
            ("read_file", ToolKind::Read, "src/parser.rs"),
        ]
        .iter()
        .enumerate()
        {
            pause(300).await;
            let mut c = tool(
                &format!("s{i}"),
                n,
                *k,
                t,
                serde_json::json!({}),
                ToolStatus::Completed,
                Some("ok"),
            );
            c.parent_id = Some("t1".into());
            let _ = ev.send(Event::Tool(c));
        }
        let _ = ev.send(Event::Tool(ToolCall {
            status: ToolStatus::Completed,
            output: Some("The parser lives in src/parser.rs".into()),
            ..parent
        }));
        stream(&ev, "The subagent found the parser in `src/parser.rs`.").await;
    } else if p.contains("tools") {
        stream(&ev, "I'll look around first. ").await;
        let b = tool(
            "1",
            "execute",
            ToolKind::Execute,
            "ls -la",
            serde_json::json!({"command": "ls -la", "description": "List files"}),
            ToolStatus::Running,
            None,
        );
        let _ = ev.send(Event::Tool(b.clone()));
        pause(300).await;
        let _ = ev.send(Event::Tool(ToolCall { status: ToolStatus::Completed, output: Some("total 12\ndrwxr-xr-x 3 me me 4096 Oct  5 13:15 .\n-rw-r--r-- 1 me me   21 Oct  5 13:15 README.md\ndrwxr-xr-x 2 me me 4096 Oct  5 13:15 src".into()), ..b }));
        let _ = ev.send(Event::Tool(tool(
            "2",
            "read_file",
            ToolKind::Read,
            "README.md",
            serde_json::json!({"path": "README.md"}),
            ToolStatus::Completed,
            Some("# demo\nhello"),
        )));
        let _ = ev.send(Event::Tool(tool(
            "3",
            "search_files",
            ToolKind::Search,
            "println",
            serde_json::json!({"pattern": "println"}),
            ToolStatus::Completed,
            Some("src/main.rs:2"),
        )));
        let mut e = tool(
            "4",
            "edit_file",
            ToolKind::Edit,
            "src/main.rs",
            serde_json::json!({"path": "src/main.rs"}),
            ToolStatus::Completed,
            Some("edited"),
        );
        e.diff = Some(FileDiff {
            path: "src/main.rs".into(),
            old: Some("fn main() {\n    println!(\"hi\");\n}\n".into()),
            new: "fn main() {\n    let name = \"world\";\n    println!(\"hello, {name}\");\n}\n"
                .into(),
        });
        let _ = ev.send(Event::Tool(e));
        let mut w = tool(
            "5",
            "write_file",
            ToolKind::Edit,
            "notes.txt",
            serde_json::json!({"path": "notes.txt"}),
            ToolStatus::Completed,
            Some("written"),
        );
        w.diff = Some(FileDiff {
            path: "notes.txt".into(),
            old: None,
            new: "todo: add tests\n".into(),
        });
        let _ = ev.send(Event::Tool(w));
        if todo_tool {
            let _ = ev.send(Event::Tool(tool(
                "6",
                "todo",
                ToolKind::Other,
                "todo",
                serde_json::json!({"action": "write", "items": [{"content": "Read the code", "status": "completed"}, {"content": "Fix greeting", "status": "in_progress"}, {"content": "Add tests", "status": "pending"}]}),
                ToolStatus::Completed,
                Some("todo list updated: 1/3 done\n\u{2713} Read the code\n\u{25b8} Fix greeting\n\u{2610} Add tests"),
            )));
        }
        let _ = ev.send(Event::Todos(vec![
            Todo {
                text: "Read the code".into(),
                status: TodoStatus::Completed,
            },
            Todo {
                text: "Fix greeting".into(),
                status: TodoStatus::InProgress,
            },
            Todo {
                text: "Add tests".into(),
                status: TodoStatus::Pending,
            },
        ]));
        pause(200).await;
        stream(
            &ev,
            "Done. I changed the greeting in `src/main.rs` and noted a follow up in `notes.txt`.",
        )
        .await;
    } else {
        stream(
            &ev,
            "Hello. This is the mock backend, so nothing here came from a model.",
        )
        .await;
    }
    let _ = ev.send(Event::Usage(Usage {
        input_tokens: 11_200,
        output_tokens: 640,
        cached_tokens: 8_000,
        context_tokens: 11_840,
        context_window: 200_000,
        cost_usd: Some(0.0412),
    }));
    let _ = ev.send(Event::TurnEnd(StopReason::EndTurn));
}

async fn rich(ev: &mpsc::UnboundedSender<Event>) {
    stream(ev, "Running a few things. ").await;
    let send = |t: ToolCall| {
        let _ = ev.send(Event::Tool(t));
    };
    let done =
        |id: &str, name: &str, kind: ToolKind, title: &str, input: serde_json::Value, out: &str| {
            tool(
                id,
                name,
                kind,
                title,
                input,
                ToolStatus::Completed,
                Some(out),
            )
        };
    send(done("r1", "Bash", ToolKind::Execute, "ls --color=always", serde_json::json!({"command": "ls --color=always"}), "\u{1b}[1;34msrc\u{1b}[0m\n\u{1b}[0;32mbuild.sh\u{1b}[0m\nCargo.toml\n\u{1b}[1;34mtests\u{1b}[0m"));
    pause(80).await;
    send(tool(
        "r2",
        "Bash",
        ToolKind::Execute,
        "make all",
        serde_json::json!({"command": "make all"}),
        ToolStatus::Failed,
        Some("Exit code 2\nmake: *** No rule to make target 'all'.  Stop."),
    ));
    send(done(
        "r3",
        "Bash",
        ToolKind::Execute,
        "curl -s api.example.com/items",
        serde_json::json!({"command": "curl -s api.example.com/items"}),
        &format!(
            "{{\"items\":[{}]}}\ndone",
            (0..60)
                .map(|i| format!("{{\"id\":{i},\"name\":\"item-{i}\"}}"))
                .collect::<Vec<_>>()
                .join(",")
        ),
    ));
    send(done(
        "r4",
        "Bash",
        ToolKind::Execute,
        "mkdir -p build/out",
        serde_json::json!({"command": "mkdir -p build/out"}),
        "",
    ));
    send(done("r5", "Bash", ToolKind::Execute, "cat logo.png", serde_json::json!({"command": "cat logo.png"}), "\u{fffd}PNG\r\n\u{1a}\n\u{0}\u{0}\u{0}\rIHDR\u{0}\u{0}\u{1}\u{0}\u{0}\u{0}\u{1}\u{0}\u{8}\u{6}\u{fffd}\u{fffd}\u{fffd}"));
    let long: String = (1..=140)
        .map(|i| format!("{i:>4}  line {i} of the file\n"))
        .collect();
    send(done(
        "r6",
        "Read",
        ToolKind::Read,
        "src/render.rs",
        serde_json::json!({"file_path": "src/render.rs"}),
        &long,
    ));
    send(done(
        "r7",
        "Grep",
        ToolKind::Search,
        "wrap(",
        serde_json::json!({"pattern": "wrap("}),
        &(1..=12)
            .map(|i| format!("src/f{i}.rs:{}: wrap(line)\n", i * 7))
            .collect::<String>(),
    ));
    send(done(
        "r8",
        "WebFetch",
        ToolKind::Fetch,
        "https://example.com/docs",
        serde_json::json!({"url": "https://example.com/docs"}),
        "The page describes the public API.\nIt has three sections.",
    ));
    send(done(
        "r9",
        "mcp__brave__browser_click",
        ToolKind::Other,
        "brave:browser_click",
        serde_json::json!({"selector": "#go"}),
        "Clicked #go",
    ));
    send(tool(
        "r10",
        "Edit",
        ToolKind::Edit,
        "src/main.rs",
        serde_json::json!({"file_path": "src/main.rs"}),
        ToolStatus::Failed,
        Some("The user denied this tool call and said: use a flag instead"),
    ));
    stream(ev, "That is the full range of output.").await;
}

async fn agents(ev: &mpsc::UnboundedSender<Event>) {
    stream(ev, "Splitting the survey in two. ").await;
    let mk = |id: &str, desc: &str| {
        tool(
            id,
            "Agent",
            ToolKind::Think,
            desc,
            serde_json::json!({"description": desc, "subagent_type": "Explore"}),
            ToolStatus::Running,
            None,
        )
    };
    let a = mk("ag1", "survey callers of wrap()");
    let b = mk("ag2", "find the test layout");
    let _ = ev.send(Event::Tool(a.clone()));
    let _ = ev.send(Event::Tool(b.clone()));
    let kid = |id: &str,
               parent: &str,
               name: &str,
               kind: ToolKind,
               title: &str,
               out: &str,
               running: bool| {
        let mut c = tool(
            id,
            name,
            kind,
            title,
            serde_json::json!({}),
            if running {
                ToolStatus::Running
            } else {
                ToolStatus::Completed
            },
            Some(out),
        );
        c.parent_id = Some(parent.into());
        Event::Tool(c)
    };
    let steps: Vec<Event> = vec![
        kid(
            "a1",
            "ag1",
            "Grep",
            ToolKind::Search,
            "wrap(",
            "src/a.rs:4\nsrc/b.rs:9\nsrc/c.rs:2",
            false,
        ),
        kid(
            "b1",
            "ag2",
            "Glob",
            ToolKind::Search,
            "tests/**/*.rs",
            "tests/a.rs\ntests/b.rs",
            false,
        ),
        kid(
            "a2",
            "ag1",
            "Read",
            ToolKind::Read,
            "src/layout.rs",
            &(1..=212).map(|i| format!("{i}\n")).collect::<String>(),
            false,
        ),
        kid(
            "b2",
            "ag2",
            "Read",
            ToolKind::Read,
            "tests/a.rs",
            "fn a() {}\n",
            false,
        ),
        kid(
            "a3",
            "ag1",
            "Read",
            ToolKind::Read,
            "src/render.rs",
            &(1..=80).map(|i| format!("{i}\n")).collect::<String>(),
            false,
        ),
        kid(
            "a4",
            "ag1",
            "Bash",
            ToolKind::Execute,
            "git log --oneline -5",
            "abc1234 wrap\ndef5678 fix",
            false,
        ),
        kid(
            "b3",
            "ag2",
            "Read",
            ToolKind::Read,
            "tests/b.rs",
            "fn b() {}\n",
            true,
        ),
    ];
    for e in steps {
        pause(350).await;
        let _ = ev.send(e);
    }
    pause(600).await;
    let _ = ev.send(Event::Tool(ToolCall {
        status: ToolStatus::Completed,
        output: Some("wrap() has 3 callers; only render.rs passes a width.".into()),
        ..a
    }));
    pause(5_000).await;
    let _ = ev.send(Event::Tool(ToolCall {
        status: ToolStatus::Completed,
        output: Some("Tests live in tests/ and use the harness in tests/common.".into()),
        ..b
    }));
    stream(ev, "Both surveys are back.").await;
}
