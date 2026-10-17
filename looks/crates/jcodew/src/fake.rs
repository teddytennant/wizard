//! Scripted stand-in for wizard, selected with `JCODEW_BACKEND=mock`.
//!
//! The reference captures were made against `tools/fake_llm.py`, which picks a scenario from
//! `fake:<name>` in the prompt. This backend answers the same names, so `tools/compare.sh` can
//! send both programs the same keys and compare the screens cell by cell. The scenarios speak
//! the way wizard does: a command is an `execute` call, an edit is `edit_file` or `write_file`
//! with the snippet as the diff, a plan is the `todo` tool. Names: `text markdown long slow
//! bash bashfail edit read todo`.

use std::path::PathBuf;
use std::time::Duration;

use agent_core::{
    Backend, BackendHandle, Config, Event, FileDiff, HistoryItem, ModelOption, NoticeLevel,
    Request, SessionInfo, StopReason, Todo, TodoStatus, ToolCall, ToolKind, ToolStatus, Usage,
};
use serde_json::json;
use tokio::sync::mpsc;

pub struct FakeBackend;

type Tx = mpsc::UnboundedSender<Event>;

pub const MARKDOWN: &str = "# Heading one\n\nSome **bold** text, some *italic* text and `inline code`.\n\n## Heading two\n\n- first bullet\n- second bullet with a longer line that keeps going so that it has to wrap at the edge of a narrow terminal window\n  - nested bullet\n\n1. numbered one\n2. numbered two\n\n> a block quote\n\n```rust\nfn main() {\n    println!(\"hello\");\n}\n```\n\n| name | value |\n| ---- | ----- |\n| one  | 1     |\n| two  | 2     |\n\nA [link](https://example.com) and a rule:\n\n---\n\nDone.";

pub fn long_text() -> String {
    (1..=30)
        .map(|i| format!("Paragraph {i}. The quick brown fox jumps over the lazy dog and keeps running through the field until the sun goes down."))
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub fn config(cwd: &std::path::Path) -> Config {
    let m = |n: &str| ModelOption {
        id: n.into(),
        name: n.into(),
        provider: "fake".into(),
    };
    Config {
        model: "grok-4.7".into(),
        models: vec![m("grok-4.7"), m("grok-4.6"), m("grok-4.7-fast")],
        cwd: cwd.display().to_string(),
        backend: "fake 0.0".into(),
        ..Default::default()
    }
}

pub fn sessions(cwd: &std::path::Path) -> Vec<SessionInfo> {
    let s = |id: &str, title: &str, ago: i64| SessionInfo {
        id: id.into(),
        title: title.into(),
        cwd: cwd.display().to_string(),
        updated: 1_790_000_000 - ago,
    };
    vec![
        s("fake-session-1", "Fix the flaky parser test", 600),
        s("fake-session-2", "Add a --json flag", 90_000),
    ]
}

/// `fake:name` anywhere in the prompt gives the scenario name.
pub fn scenario_name(prompt: &str) -> &str {
    match prompt.find("fake:") {
        Some(at) => {
            let rest = &prompt[at + 5..];
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            &rest[..end]
        }
        None => "text",
    }
}

fn call(id: &str, name: &str, kind: ToolKind, title: &str, input: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        kind,
        title: title.into(),
        input,
        status: ToolStatus::Running,
        ..Default::default()
    }
}

/// One thing a scripted turn does.
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Step {
    Text(String),
    /// A call that runs and then finishes with this output.
    Tool {
        call: ToolCall,
        output: String,
        ok: bool,
        diff: Option<FileDiff>,
    },
    Todos(Vec<Todo>),
    Sleep(Duration),
}

fn done(call: ToolCall, output: &str) -> Step {
    Step::Tool {
        call,
        output: output.to_string(),
        ok: true,
        diff: None,
    }
}

fn edit(call: ToolCall, output: String, diff: FileDiff) -> Step {
    Step::Tool {
        call,
        output,
        ok: true,
        diff: Some(diff),
    }
}

/// What the turn for `prompt` does, in order.
pub fn steps(cwd: &std::path::Path, prompt: &str) -> Vec<Step> {
    let t = |s: &str| Step::Text(s.to_string());
    let notes = cwd.join("notes.txt").display().to_string();
    match scenario_name(prompt) {
        "markdown" => vec![t(MARKDOWN)],
        "long" => vec![Step::Text(long_text())],
        "slow" => vec![
            Step::Sleep(Duration::from_secs(30)),
            t("Slow reply finished."),
        ],
        "bash" => {
            let cmd = "printf 'alpha\\nbeta\\ngamma\\n'";
            let c = call(
                "call_bash_1",
                "execute",
                ToolKind::Execute,
                cmd,
                json!({"command": cmd}),
            );
            vec![
                t("Listing the directory."),
                done(c, "alpha\nbeta\ngamma\n"),
                t("Three lines were printed."),
            ]
        }
        "bashfail" => {
            let cmd = "sh -c 'echo boom >&2; exit 3'";
            let c = call(
                "call_fail_1",
                "execute",
                ToolKind::Execute,
                cmd,
                json!({"command": cmd}),
            );
            vec![
                Step::Tool {
                    call: c,
                    output: "boom\n\nExit code: 3".into(),
                    ok: false,
                    diff: None,
                },
                t("The command failed."),
            ]
        }
        "edit" => {
            let body = "one\ntwo\nthree\n";
            let write = call(
                "call_write_1",
                "write_file",
                ToolKind::Edit,
                &notes,
                json!({"path": notes, "content": body}),
            );
            let change = call(
                "call_edit_1",
                "edit_file",
                ToolKind::Edit,
                &notes,
                json!({"path": notes, "old_string": "two", "new_string": "TWO\ntwo and a half"}),
            );
            vec![
                t("Creating the file."),
                edit(
                    write,
                    format!("Created {notes} (3 lines)"),
                    FileDiff {
                        path: notes.clone(),
                        old: None,
                        new: body.into(),
                    },
                ),
                t("Now changing the second line."),
                edit(
                    change,
                    format!("Edited {notes}"),
                    FileDiff {
                        path: notes.clone(),
                        old: Some("two".into()),
                        new: "TWO\ntwo and a half".into(),
                    },
                ),
                t("Edited `notes.txt`."),
            ]
        }
        "read" => {
            let c = call(
                "call_read_1",
                "read_file",
                ToolKind::Read,
                &notes,
                json!({"path": notes}),
            );
            vec![
                done(c, "one\nTWO\ntwo and a half\nthree\n"),
                t("Read the file."),
            ]
        }
        "todo" => {
            let todos: Vec<Todo> = [
                ("Read the code", TodoStatus::Completed),
                ("Write the patch", TodoStatus::InProgress),
                ("Run the tests", TodoStatus::Pending),
            ]
            .iter()
            .map(|(text, status)| Todo {
                text: (*text).into(),
                status: *status,
            })
            .collect();
            let c = call(
                "call_todo_1",
                "todo",
                ToolKind::Think,
                "Plan the work",
                json!({"todos": todos.iter().map(|t| json!({"content": t.text})).collect::<Vec<_>>()}),
            );
            vec![
                Step::Todos(todos),
                done(c, "Updated todos"),
                t("Plan recorded."),
            ]
        }
        "text" => vec![t("Hello from the scripted model. This is a plain reply.")],
        other => vec![Step::Text(format!("unknown scenario {other}"))],
    }
}

/// The events a step turns into, without the pauses between them.
pub fn events(step: &Step) -> Vec<Event> {
    match step {
        Step::Text(s) => vec![Event::TextDelta(s.clone())],
        Step::Tool {
            call,
            output,
            ok,
            diff,
        } => {
            let mut end = call.clone();
            end.status = if *ok {
                ToolStatus::Completed
            } else {
                ToolStatus::Failed
            };
            end.output = Some(output.clone());
            end.diff = diff.clone();
            vec![Event::Tool(call.clone()), Event::Tool(end)]
        }
        Step::Todos(t) => vec![Event::Todos(t.clone())],
        Step::Sleep(_) => Vec::new(),
    }
}

async fn scenario(tx: Tx, cwd: PathBuf, prompt: String) {
    let _ = tx.send(Event::TurnStart);
    tokio::time::sleep(Duration::from_millis(200)).await;
    for step in steps(&cwd, &prompt) {
        match &step {
            Step::Sleep(d) => tokio::time::sleep(*d).await,
            Step::Text(s) => {
                let chars: Vec<char> = s.chars().collect();
                for chunk in chars.chunks(24) {
                    let _ = tx.send(Event::TextDelta(chunk.iter().collect()));
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
            Step::Tool { .. } => {
                let evs = events(&step);
                let _ = tx.send(evs[0].clone());
                tokio::time::sleep(Duration::from_millis(300)).await;
                let _ = tx.send(evs[1].clone());
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Step::Todos(_) => {
                for e in events(&step) {
                    let _ = tx.send(e);
                }
            }
        }
    }
}

impl Backend for FakeBackend {
    fn spawn(cwd: PathBuf, _resume: Option<String>) -> anyhow::Result<BackendHandle> {
        let (req_tx, mut req_rx) = mpsc::unbounded_channel::<Request>();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel::<Event>();
        tokio::spawn(async move {
            let mut cfg = config(&cwd);
            let mut session = 1u32;
            let mut usage = Usage {
                context_window: 200_000,
                ..Default::default()
            };
            let ready = |tx: &Tx, n: u32, cfg: &Config| {
                let _ = tx.send(Event::Ready {
                    session_id: format!("fake-session-{n}"),
                    config: cfg.clone(),
                });
            };
            ready(&ev_tx, session, &cfg);
            let _ = ev_tx.send(Event::Commands(agent_core::mock::commands()));
            let mut running: Option<tokio::task::JoinHandle<()>> = None;
            loop {
                let req = match running.as_mut() {
                    Some(job) => tokio::select! {
                        r = req_rx.recv() => r,
                        _ = job => {
                            running = None;
                            usage.input_tokens += 1200;
                            usage.output_tokens += 34;
                            usage.context_tokens = 1200;
                            let _ = ev_tx.send(Event::Usage(usage.clone()));
                            let _ = ev_tx.send(Event::TurnEnd(StopReason::EndTurn));
                            continue;
                        }
                    },
                    None => req_rx.recv().await,
                };
                let Some(req) = req else { break };
                match req {
                    Request::Prompt(p)
                    | Request::Steer(p)
                    | Request::PromptWith { text: p, .. } => {
                        if let Some(job) = running.take() {
                            job.abort();
                            let _ = ev_tx.send(Event::TurnEnd(StopReason::Cancelled));
                        }
                        if let Some(rest) = p.strip_prefix('/') {
                            let name = rest.split_whitespace().next().unwrap_or("");
                            let _ = ev_tx.send(Event::TurnStart);
                            let _ = ev_tx.send(Event::Notice {
                                level: NoticeLevel::Info,
                                text: format!("/{name}: answered by the scripted backend"),
                            });
                            let _ = ev_tx.send(Event::TurnEnd(StopReason::EndTurn));
                        } else {
                            running = Some(tokio::spawn(scenario(ev_tx.clone(), cwd.clone(), p)));
                        }
                    }
                    Request::Cancel => {
                        if let Some(job) = running.take() {
                            job.abort();
                            let _ = ev_tx.send(Event::TurnEnd(StopReason::Cancelled));
                        }
                    }
                    Request::NewSession => {
                        session += 1;
                        usage = Usage {
                            context_window: 200_000,
                            ..Default::default()
                        };
                        ready(&ev_tx, session, &cfg);
                    }
                    Request::ListSessions => {
                        let _ = ev_tx.send(Event::Sessions(sessions(&cwd)));
                    }
                    Request::LoadSession(id) => {
                        let _ = ev_tx.send(Event::History {
                            session_id: id.clone(),
                            items: vec![
                                HistoryItem::User("an earlier prompt".into()),
                                HistoryItem::Assistant("An earlier reply.".into()),
                            ],
                        });
                        let _ = ev_tx.send(Event::Ready {
                            session_id: id,
                            config: cfg.clone(),
                        });
                    }
                    Request::SetModel(m) => {
                        cfg.model = m;
                        let _ = ev_tx.send(Event::ConfigChanged(cfg.clone()));
                    }
                    Request::SetEffort(e) => {
                        cfg.effort = e;
                        let _ = ev_tx.send(Event::ConfigChanged(cfg.clone()));
                    }
                    Request::Shutdown => break,
                    _ => {}
                }
            }
        });
        Ok(BackendHandle {
            tx: req_tx,
            rx: ev_rx,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenario_names() {
        assert_eq!(scenario_name("please fake:edit now"), "edit");
        assert_eq!(scenario_name("hello"), "text");
    }
}
