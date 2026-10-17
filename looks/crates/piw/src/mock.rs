// OWNER: shared (scripted backend for screenshots and tests)
//! `PIW_BACKEND=mock`: no process, no model. A prompt containing `[[name]]` plays a scenario that
//! mirrors one of `tools/fake-llm/openai-chat/scenarios.py` against the demo project that
//! `tools/piw-shots.sh` builds, so a screen can be compared with the real Pi's capture of the
//! same state. Anything else falls through to `agent_core`'s mock backend.

use std::time::Duration;

use agent_core::mock::MockBackend;
use agent_core::*;
use serde_json::json;
use tokio::sync::mpsc;

pub struct PiMock;

pub fn config() -> Config {
    let m = |p: &str, n: &str| ModelOption {
        id: format!("{p}/{n}"),
        name: n.into(),
        provider: p.into(),
    };
    let multi = std::env::var("PIW_MOCK_MODELS").is_ok_and(|v| v == "multi");
    let mut models = vec![m("fake", "fake-model")];
    if multi {
        models.push(m("other", "small-model"));
        models.push(m("other", "big-model"));
    }
    Config {
        model: "fake/fake-model".into(),
        models,
        effort: "medium".into(),
        efforts: ["low", "medium", "high", "xhigh"]
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

const MD: &str = "# Heading one\n\nSome **bold**, some *italic*, some ~~strike~~, and `inline code` in a paragraph with a [link](https://example.com/docs) and a bare https://example.com/bare url.\n\n## Heading two\n\n### Heading three\n\n- first bullet\n- second bullet with `code`\n  - nested bullet\n  - another nested\n1. numbered one\n2. numbered two\n\n> A block quote that\n> spans two lines.\n\n```rust\nfn main() {\n    // say hi\n    let n: u32 = 42;\n    println!(\"hi {}\", n);\n}\n```\n\n```python\ndef f(x):\n    return [i * 2 for i in range(x)]  # doubles\n```\n\n```\nplain fence with no language\n```\n\n| name | qty | note |\n|------|----:|------|\n| apple | 3 | red |\n| banana | 12 | yellow and long enough to wrap in a narrow terminal window |\n\n---\n\nLast paragraph after a rule.\n";

fn call(id: &str, name: &str, kind: ToolKind, title: &str, input: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        kind,
        title: title.into(),
        input,
        status: ToolStatus::Running,
        output: None,
        diff: None,
        parent_id: None,
    }
}

fn done(mut c: ToolCall, out: &str) -> ToolCall {
    c.status = ToolStatus::Completed;
    c.output = Some(out.into());
    c
}

fn failed(mut c: ToolCall, out: &str) -> ToolCall {
    c.status = ToolStatus::Failed;
    c.output = Some(out.into());
    c
}

fn long_lines(n: usize, f: impl Fn(usize) -> String) -> String {
    (1..=n).map(f).collect::<Vec<_>>().join("\n")
}

enum Step {
    Say(String),
    Think(String),
    /// A call, its result, and how long it runs in ms.
    Tool(Box<ToolCall>, Box<ToolCall>, u64),
    Fail(String),
    Hang(u64),
}

use Step::*;

fn scenario(name: &str) -> Vec<Step> {
    let ls = || {
        let c = call(
            "c1",
            "list_files",
            ToolKind::Read,
            ".",
            json!({"path": "."}),
        );
        let d = done(
            c.clone(),
            ".git/\nAGENTS.md\ndata/\npic.png\nREADME.md\nsrc/",
        );
        Tool(Box::new(c), Box::new(d), 30)
    };
    let read_readme = || {
        let c = call(
            "c2",
            "read_file",
            ToolKind::Read,
            "README.md",
            json!({"path": "README.md"}),
        );
        let d = done(c.clone(), "# demo project\nhello\n");
        Tool(Box::new(c), Box::new(d), 30)
    };
    let grep_main = || {
        let c = call(
            "c3",
            "search_files",
            ToolKind::Search,
            "main",
            json!({"pattern": "main", "path": "src"}),
        );
        let d = done(c.clone(), "main.rs:1: fn main() {");
        Tool(Box::new(c), Box::new(d), 30)
    };
    let find_rs = || {
        let c = call(
            "c4",
            "find",
            ToolKind::Search,
            "*.rs",
            json!({"pattern": "*.rs", "path": "."}),
        );
        let d = done(c.clone(), "src/main.rs");
        Tool(Box::new(c), Box::new(d), 30)
    };
    let bash_status = || {
        let c = call(
            "c5",
            "execute",
            ToolKind::Execute,
            "git status --short | head -5; echo done",
            json!({"command": "git status --short | head -5; echo done"}),
        );
        let d = done(c.clone(), "?? AGENTS.md\n?? pic.png\ndone");
        Tool(Box::new(c), Box::new(d), 30)
    };
    let write_notes = || {
        let mut c = call(
            "c6",
            "write_file",
            ToolKind::Edit,
            "notes.txt",
            json!({"path": "notes.txt", "content": "hi\nsecond line\nthird line\n"}),
        );
        c.diff = Some(FileDiff {
            path: "notes.txt".into(),
            old: None,
            new: "hi\nsecond line\nthird line\n".into(),
        });
        let d = done(c.clone(), "Successfully wrote 25 bytes to notes.txt");
        Tool(Box::new(c), Box::new(d), 30)
    };
    let edit_main = |new: &str| {
        let old = "println!(\"hi\");";
        let mut c = call(
            "c7",
            "edit_file",
            ToolKind::Edit,
            "src/main.rs",
            json!({"path": "src/main.rs", "old_string": old, "new_string": new}),
        );
        c.diff = Some(FileDiff {
            path: "src/main.rs".into(),
            old: Some(old.into()),
            new: new.into(),
        });
        let d = done(c.clone(), "Successfully replaced text in src/main.rs.");
        Tool(Box::new(c), Box::new(d), 30)
    };
    let n = name;
    match n {
        "hello" => vec![Say("Hello! I can read, write and edit files in this project. What should I work on?".into())],
        "md" => vec![Say(MD.into())],
        "think" => vec![
            Think("The user wants a short answer. I should check the README first, then answer briefly. Two sentences are enough here.\n\nSecond paragraph of thinking to see wrapping.".into()),
            Say("Done thinking. The answer is **42**.".into()),
        ],
        "tools" => vec![
            Say("I'll look around first.".into()),
            ls(),
            read_readme(),
            grep_main(),
            find_rs(),
            bash_status(),
            write_notes(),
            edit_main("println!(\"hello, world\");\n    println!(\"bye\");"),
            Say("All done: listed the project, read the README, wrote `notes.txt` and edited `src/main.rs`.".into()),
        ],
        "s-ls" => vec![ls(), Say("Listed the directory.".into())],
        "s-read" => {
            let c = call("c2", "read_file", ToolKind::Read, "src/main.rs", json!({"path": "src/main.rs"}));
            let d = done(c.clone(), "fn main() {\n    println!(\"hi\");\n}\n");
            vec![Tool(Box::new(c), Box::new(d), 30), Say("Read the file.".into())]
        }
        "s-grep" => {
            let c = call(
                "c3",
                "search_files",
                ToolKind::Search,
                "line 1",
                json!({"pattern": "line 1", "path": "data", "glob": "*.txt", "limit": 5}),
            );
            let d = done(
                c.clone(),
                "long.txt:1: line 1 of the long file\nlong.txt:10: line 10 of the long file\nlong.txt:11: line 11 of the long file\nlong.txt:12: line 12 of the long file\nlong.txt:13: line 13 of the long file\n\n[5 matches limit reached. Use limit=10 for more, or refine pattern]",
            );
            vec![Tool(Box::new(c), Box::new(d), 30), Say("Searched.".into())]
        }
        "s-find" => vec![find_rs(), Say("Found files.".into())],
        "s-bash" => {
            let c = call(
                "c5",
                "execute",
                ToolKind::Execute,
                "echo hello; ls src",
                json!({"command": "echo hello; ls src", "timeout": 20}),
            );
            let d = done(c.clone(), "hello\nmain.rs");
            vec![Tool(Box::new(c), Box::new(d), 30), Say("Ran the command.".into())]
        }
        "s-write" => vec![write_notes(), Say("Wrote the file.".into())],
        "s-edit" => vec![edit_main("println!(\"hello, world\");"), Say("Edited the file.".into())],
        "bashlong" => {
            let cmd = "for i in $(seq 1 30); do echo \"line $i of output\"; done";
            let c = call("c5", "execute", ToolKind::Execute, cmd, json!({"command": cmd}));
            let d = done(c.clone(), &long_lines(30, |i| format!("line {i} of output")));
            vec![Tool(Box::new(c), Box::new(d), 30), Say("The loop printed 30 lines.".into())]
        }
        "bashfail" => {
            let cmd = "echo partial; echo oops >&2; exit 3";
            let c = call("c5", "execute", ToolKind::Execute, cmd, json!({"command": cmd}));
            let d = failed(c.clone(), "partial\noops\n\n\nCommand exited with code 3");
            vec![Tool(Box::new(c), Box::new(d), 30), Say("The command failed with exit 3.".into())]
        }
        "readlong" => {
            let c = call("c2", "read_file", ToolKind::Read, "data/long.txt", json!({"path": "data/long.txt"}));
            let d = done(c.clone(), &long_lines(60, |i| format!("line {i} of the long file")));
            vec![Tool(Box::new(c), Box::new(d), 30), Say("Read the long file.".into())]
        }
        "readrange" => {
            let c = call(
                "c2",
                "read_file",
                ToolKind::Read,
                "data/long.txt",
                json!({"path": "data/long.txt", "offset": 10, "limit": 5}),
            );
            let d = done(
                c.clone(),
                &(10..15).map(|i| format!("line {i} of the long file")).collect::<Vec<_>>().join("\n"),
            );
            vec![Tool(Box::new(c), Box::new(d), 30), Say("Read lines 10 to 14.".into())]
        }
        "readmissing" => {
            let c = call("c2", "read_file", ToolKind::Read, "nope/missing.txt", json!({"path": "nope/missing.txt"}));
            let d = failed(c.clone(), "ENOENT: no such file or directory, access '/home/me/demo/nope/missing.txt'");
            vec![Tool(Box::new(c), Box::new(d), 30), Say("That file does not exist.".into())]
        }
        "writelong" => {
            let content: String = (1..=8).map(|i| format!("def f{i}(x):\n    return x + {i}\n\n")).collect();
            let content = content.trim_end_matches('\n').to_string() + "\n";
            let mut c = call("c6", "write_file", ToolKind::Edit, "data/out.py", json!({"path": "data/out.py", "content": content}));
            c.diff = Some(FileDiff { path: "data/out.py".into(), old: None, new: content.clone() });
            let d = done(c.clone(), "Successfully wrote to data/out.py");
            vec![Tool(Box::new(c), Box::new(d), 30), Say("Wrote data/out.py.".into())]
        }
        "editmulti" => {
            let old = long_lines(60, |i| format!("line {i} of the long file")) + "\n";
            let new = old
                .replace("line 3 of the long file", "line THREE of the long file")
                .replace(
                    "line 20 of the long file",
                    "line TWENTY of the long file\nand an inserted one",
                )
                .replace("line 41 of the long file", "line 41 changed");
            let mut c = call(
                "c7",
                "edit_file",
                ToolKind::Edit,
                "data/long.txt",
                json!({"path": "data/long.txt"}),
            );
            c.diff = Some(FileDiff {
                path: "data/long.txt".into(),
                old: Some(old),
                new,
            });
            let d = done(c.clone(), "Successfully replaced 3 blocks in data/long.txt.");
            vec![Tool(Box::new(c), Box::new(d), 30), Say("Edited three places.".into())]
        }
        "editfail" => {
            let mut c = call(
                "c7",
                "edit_file",
                ToolKind::Edit,
                "src/main.rs",
                json!({"path": "src/main.rs", "old_string": "does not exist anywhere", "new_string": "x"}),
            );
            c.diff = Some(FileDiff { path: "src/main.rs".into(), old: Some("does not exist anywhere".into()), new: "x".into() });
            let d = failed(c.clone(), "Could not find the exact text in src/main.rs. The old text must match exactly including all whitespace and newlines.");
            vec![Tool(Box::new(c), Box::new(d), 30), Say("The edit did not match.".into())]
        }
        "grepmany" => {
            let g = call("c3", "search_files", ToolKind::Search, "line", json!({"pattern": "line", "path": "data"}));
            let gd = done(g.clone(), &long_lines(20, |i| format!("long.txt:{i}: line {i} of the long file")));
            let l = call("c8", "list_files", ToolKind::Read, "data", json!({"path": "data"}));
            let ld = done(l.clone(), "long.txt");
            vec![Tool(Box::new(g), Box::new(gd), 30), find_rs(), Tool(Box::new(l), Box::new(ld), 30), Say("Searched.".into())]
        }
        "slow" => vec![Say("This answer streams slowly so the working indicator, queue and abort can be observed. ".repeat(4)), Hang(0)],
        "slowtool" => {
            let mut c = call("c5", "execute", ToolKind::Execute, "echo start; sleep 25; echo end", json!({"command": "echo start; sleep 25; echo end"}));
            c.output = Some("start".into());
            let d = done(c.clone(), "start\nend");
            vec![Tool(Box::new(c), Box::new(d), 25_000), Say("Finished sleeping.".into())]
        }
        // a write whose call is on screen while it runs, like Pi's `[[slowwrite]]` scenario
        "slowwrite" => {
            let content: String = (1..15).map(|i| format!("def g{i}(x):\n    return x * {i}\n\n")).collect();
            let mut c = call("c7", "write_file", ToolKind::Edit, "data/streamed.py", json!({"path": "data/streamed.py", "content": content}));
            c.diff = Some(FileDiff { path: "data/streamed.py".into(), old: None, new: content.clone() });
            let d = done(c.clone(), "Successfully wrote to data/streamed.py");
            vec![Tool(Box::new(c), Box::new(d), 8_000), Say("Done.".into())]
        }
        "slowthink" => vec![Think("Pondering at length about the meaning of the question. ".repeat(6)), Hang(25_000), Say("Okay.".into())],
        "longline" => vec![Say(format!(
            "A very long unbroken token: {}\n\nAnd a normal paragraph after it that should wrap at the terminal width like any other paragraph does in the transcript.",
            "x".repeat(300)
        ))],
        "img" => {
            let c = call("c2", "read_file", ToolKind::Read, "pic.png", json!({"path": "pic.png"}));
            let d = done(c.clone(), "Read image file [image/png]");
            vec![Tool(Box::new(c), Box::new(d), 30), Say("That is a small image.".into())]
        }
        "apierror" => vec![Fail("400: {\"message\":\"scripted bad request: context length exceeded\",\"type\":\"server_error\"}".into())],
        "apierror500" => vec![Fail("500: {\"message\":\"scripted upstream failure\",\"type\":\"server_error\"}".into())],
        _ => vec![Say(format!("(mock) no scenario named {n}"))],
    }
}

/// `[[name]]` anywhere in the prompt.
pub fn scenario_name(prompt: &str) -> Option<&str> {
    let a = prompt.find("[[")?;
    let b = prompt[a + 2..].find("]]")?;
    Some(&prompt[a + 2..a + 2 + b])
}

async fn pause(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

async fn stream(ev: &mpsc::UnboundedSender<Event>, text: &str, think: bool, slow: bool) {
    let step = if slow { 6 } else { 40 };
    let chars: Vec<char> = text.chars().collect();
    for chunk in chars.chunks(step) {
        let s: String = chunk.iter().collect();
        let _ = ev.send(if think {
            Event::ThoughtDelta(s)
        } else {
            Event::TextDelta(s)
        });
        pause(if slow { 400 } else { 4 }).await;
    }
}

async fn play(prompt: String, ev: mpsc::UnboundedSender<Event>, tokens: u64) {
    let name = scenario_name(&prompt).unwrap_or("").to_string();
    let slow = name.starts_with("slow");
    let _ = ev.send(Event::TurnStart);
    let mut reason = StopReason::EndTurn;
    for step in scenario(&name) {
        match step {
            Say(t) => stream(&ev, &t, false, slow).await,
            Think(t) => stream(&ev, &t, true, slow).await,
            Tool(call, result, ms) => {
                let _ = ev.send(Event::Tool(*call));
                pause(ms).await;
                let _ = ev.send(Event::Tool(*result));
            }
            Fail(msg) => {
                let _ = ev.send(Event::Notice {
                    level: NoticeLevel::Error,
                    text: msg,
                });
                reason = StopReason::Error;
                break;
            }
            Hang(ms) => pause(if ms == 0 { 30_000 } else { ms }).await,
        }
    }
    let _ = ev.send(Event::Usage(Usage {
        input_tokens: 400 + tokens * 3,
        output_tokens: 20 + tokens,
        context_tokens: 400 + tokens * 3,
        context_window: 200_000,
        ..Default::default()
    }));
    let _ = ev.send(Event::TurnEnd(reason));
}

impl Backend for PiMock {
    fn spawn(cwd: std::path::PathBuf, resume: Option<String>) -> anyhow::Result<BackendHandle> {
        let mut inner = MockBackend::spawn(cwd, resume)?;
        let (tx, mut req_rx) = mpsc::unbounded_channel::<Request>();
        let (ev_tx, rx) = mpsc::unbounded_channel::<Event>();
        let fwd = ev_tx.clone();
        tokio::spawn(async move {
            let mut cfg = config();
            while let Some(e) = inner.rx.recv().await {
                let e = match e {
                    Event::Ready { session_id, .. } => Event::Ready {
                        session_id,
                        config: cfg.clone(),
                    },
                    // piw keeps the model and effort of its own config
                    Event::ConfigChanged(_) => continue,
                    e => e,
                };
                cfg.cwd.clear();
                if fwd.send(e).is_err() {
                    break;
                }
            }
        });
        let inner_tx = inner.tx.clone();
        tokio::spawn(async move {
            let mut cfg = config();
            let mut running: Option<tokio::task::JoinHandle<()>> = None;
            let mut n = 0u64;
            let mut first_prompt: Option<String> = None;
            while let Some(req) = req_rx.recv().await {
                if let Request::Prompt(t) | Request::PromptWith { text: t, .. } = &req {
                    if first_prompt.is_none() && !t.starts_with('/') {
                        first_prompt = Some(t.clone());
                    }
                }
                match req {
                    // the session list belongs to this folder, so `/resume` has rows to show
                    Request::ListSessions => {
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map_or(0, |d| d.as_secs() as i64);
                        let cwd = cfg.cwd.clone();
                        let s = |id: &str, t: &str, ago: i64| SessionInfo {
                            id: id.into(),
                            title: t.into(),
                            cwd: cwd.clone(),
                            updated: now - ago,
                        };
                        let mut v = Vec::new();
                        if let Some(t) = &first_prompt {
                            v.push(s("mock-1", t, 5));
                        }
                        v.push(s("a", "Fix the flaky parser test", 600));
                        v.push(s("b", "Add a --json flag", 90_000));
                        let _ = ev_tx.send(Event::Sessions(v));
                    }
                    Request::Prompt(t) | Request::PromptWith { text: t, .. }
                        if scenario_name(&t).is_some() =>
                    {
                        n += 1;
                        let ev = ev_tx.clone();
                        running = Some(tokio::spawn(play(t, ev, n * 12)));
                    }
                    Request::Prompt(t) if t.starts_with("/compact") => {
                        let ev = ev_tx.clone();
                        running = Some(tokio::spawn(async move {
                            let _ = ev.send(Event::TurnStart);
                            pause(1200).await;
                            let _ = ev.send(Event::Notice {
                                level: NoticeLevel::Info,
                                text: "Compacted from 1,261 tokens\n\n## Goal\nSummary of the earlier conversation.".into(),
                            });
                            let _ = ev.send(Event::TurnEnd(StopReason::EndTurn));
                        }));
                    }
                    Request::Cancel => {
                        if let Some(h) = running.take() {
                            if !h.is_finished() {
                                h.abort();
                                let _ = ev_tx.send(Event::TurnEnd(StopReason::Cancelled));
                            }
                        }
                        let _ = inner_tx.send(Request::Cancel);
                    }
                    Request::SetModel(m) => {
                        cfg.model = m;
                        let _ = ev_tx.send(Event::ConfigChanged(cfg.clone()));
                    }
                    Request::SetEffort(e) => {
                        cfg.effort = e;
                        let _ = ev_tx.send(Event::ConfigChanged(cfg.clone()));
                    }
                    Request::Shutdown => {
                        let _ = inner_tx.send(Request::Shutdown);
                        break;
                    }
                    other => {
                        let _ = inner_tx.send(other);
                    }
                }
            }
        });
        Ok(BackendHandle { tx, rx })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenario_names_are_found_in_prompts() {
        assert_eq!(scenario_name("hello there [[md]]"), Some("md"));
        assert_eq!(scenario_name("plain"), None);
    }

    #[test]
    fn every_known_scenario_ends_cleanly() {
        for n in [
            "hello", "md", "think", "tools", "s-ls", "s-edit", "bashfail", "apierror",
        ] {
            assert!(!scenario(n).is_empty(), "{n}");
        }
    }
}
