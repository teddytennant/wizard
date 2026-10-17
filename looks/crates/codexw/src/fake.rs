// OWNER: fake-backend (scripted scenarios for the cell and error shots)
//! Scripted stand-in for wizard, selected with `CODEXW_BACKEND=mock`.
//!
//! The Codex reference captures were made against `tools/fake-llm`, which picks a scenario from
//! `fake:<name>` in the last user message. This backend answers the same names, so
//! `tools/codexw-shots.sh` can send the same prompts and the output can be compared cell by cell.
//! The scenarios speak the way wizard does, not the way Codex's model did: a command is an
//! `execute` call, an edit is `edit_file` or `write_file` with the snippet as the diff, a plan is
//! the `todo` tool, search is `web_search`, an MCP tool is `server__tool`, and wizard has no
//! apply_patch and no delete tool (a deletion is `rm`). Calls arrive whole, `Running` first and
//! `Completed` or `Failed` a moment later.
//!
//! A prompt without `fake:` goes to `agent_core::mock`, so the keyword scenarios (`tools`, `long`,
//! `perm`, ...) keep working. Names: `text markdown long slow explore explore-sh exec exec-edge
//! exec-sleep approval approval-escalate patch patch-multi patch-add patch-delete patch-rename
//! patch-wrap patch-fail plan search search-slow mcp mcp-long mcp-slow ask perm reason-body
//! reason-two reason-variants proposed-plan worked tour compact-summary structured error-500
//! error-429 error-failed error-quota error-400 error-drop error-conn`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent_core::{
    Backend, BackendHandle, Event, FileDiff, NoticeLevel, PermissionRequest, Request, StopReason,
    Todo, TodoStatus, ToolCall, ToolKind, ToolStatus,
};
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};

pub struct FakeBackend;

type Tx = mpsc::UnboundedSender<Event>;
type Decisions = Arc<Mutex<mpsc::UnboundedReceiver<(String, bool, String)>>>;

/// `fake:name` anywhere in the prompt gives the scenario name.
pub fn scenario_name(prompt: &str) -> Option<&str> {
    let rest = &prompt[prompt.find("fake:")? + 5..];
    let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
    Some(&rest[..end])
}

impl Backend for FakeBackend {
    fn spawn(cwd: PathBuf, resume: Option<String>) -> anyhow::Result<BackendHandle> {
        let inner = agent_core::mock::MockBackend::spawn(cwd.clone(), resume)?;
        let BackendHandle {
            tx: inner_tx,
            rx: mut inner_rx,
        } = inner;
        let (tx, mut req_rx) = mpsc::unbounded_channel::<Request>();
        let (ev_tx, rx) = mpsc::unbounded_channel::<Event>();
        let fwd = ev_tx.clone();
        tokio::spawn(async move {
            while let Some(e) = inner_rx.recv().await {
                if fwd.send(e).is_err() {
                    break;
                }
            }
        });
        tokio::spawn(async move {
            let (dtx, drx) = mpsc::unbounded_channel::<(String, bool, String)>();
            let drx: Decisions = Arc::new(Mutex::new(drx));
            let mut running: Option<tokio::task::JoinHandle<()>> = None;
            let cwd = cwd.display().to_string();
            while let Some(req) = req_rx.recv().await {
                let live = running.as_ref().is_some_and(|h| !h.is_finished());
                match req {
                    Request::Prompt(text)
                    | Request::PromptWith { text, .. }
                    | Request::Steer(text)
                        if scenario_name(&text).is_some() =>
                    {
                        if let Some(h) = running.take() {
                            if !h.is_finished() {
                                h.abort();
                                let _ = ev_tx.send(Event::TurnEnd(StopReason::Cancelled));
                            }
                        }
                        let name = scenario_name(&text).unwrap_or_default().to_string();
                        let (ev, d, cwd) = (ev_tx.clone(), drx.clone(), cwd.clone());
                        running = Some(tokio::spawn(async move {
                            let _ = ev.send(Event::TurnStart);
                            let end = play(&name, &cwd, &ev, &d).await;
                            let _ = ev.send(Event::TurnEnd(end));
                        }));
                    }
                    Request::Cancel if live => {
                        if let Some(h) = running.take() {
                            h.abort();
                        }
                        let _ = ev_tx.send(Event::TurnEnd(StopReason::Cancelled));
                    }
                    Request::Decide {
                        id, allow, note, ..
                    } if live => {
                        let _ = dtx.send((id, allow, note));
                    }
                    Request::Answer { id, answers } if live => {
                        let joined = answers
                            .iter()
                            .map(|(q, a)| format!("{q}\t{a}"))
                            .collect::<Vec<_>>()
                            .join("\n");
                        let _ = dtx.send((id, true, joined));
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

// ---- steps ---------------------------------------------------------------------------------

#[allow(clippy::large_enum_variant)]
enum Step {
    Think(String),
    /// Streamed in word groups at a steady pace.
    Text(String),
    /// Streamed in `chunks` pieces, `ms` apart.
    Slow(String, usize, u64),
    /// A call that is announced running and finishes `ms` later.
    Call(ToolCall, u64),
    Todos(Vec<Todo>),
    Sleep(f32),
    /// Ask first; run `ok` when allowed, otherwise report `no` as the call's failure.
    Gate(Box<PermissionRequest>, Box<ToolCall>),
    /// A question the model asked; the answer comes back through `Request::Answer`.
    Ask(Box<PermissionRequest>, Box<ToolCall>),
    /// A provider failure: the error row, then the turn ends in error.
    Fail(String),
}

const LIB_RS: &str = "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n";

fn call(
    id: &str,
    name: &str,
    kind: ToolKind,
    title: &str,
    input: Value,
    status: ToolStatus,
    output: &str,
) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        kind,
        title: title.into(),
        input,
        status,
        output: (!output.is_empty()).then(|| output.to_string()),
        diff: None,
        parent_id: None,
    }
}

fn exec(id: &str, cmd: &str, out: &str, code: i32) -> Step {
    let (status, out) = if code == 0 {
        let out = if out.is_empty() {
            "(command succeeded with no output)".to_string()
        } else {
            out.to_string()
        };
        (ToolStatus::Completed, out)
    } else if out.is_empty() {
        (ToolStatus::Failed, format!("exit code: {code}"))
    } else {
        (ToolStatus::Failed, format!("{out}\nexit code: {code}"))
    };
    Step::Call(
        call(
            id,
            "execute",
            ToolKind::Execute,
            cmd,
            json!({ "command": cmd }),
            status,
            &out,
        ),
        300,
    )
}

fn read_file(id: &str, path: &str, body: &str) -> Step {
    let numbered: Vec<String> = body
        .lines()
        .enumerate()
        .map(|(i, l)| format!("{:>6}\t{l}", i + 1))
        .collect();
    Step::Call(
        call(
            id,
            "read_file",
            ToolKind::Read,
            path,
            json!({ "path": path }),
            ToolStatus::Completed,
            &numbered.join("\n"),
        ),
        200,
    )
}

fn edit_file(cwd: &str, id: &str, path: &str, old: &str, new: &str, fail: Option<&str>) -> Step {
    let mut c = call(
        id,
        "edit_file",
        ToolKind::Edit,
        path,
        json!({ "path": path, "old_string": old, "new_string": new }),
        ToolStatus::Completed,
        &format!("Edited {cwd}/{path}: replaced 1 occurrence"),
    );
    c.diff = Some(FileDiff {
        path: path.into(),
        old: Some(old.into()),
        new: new.into(),
    });
    if let Some(msg) = fail {
        c.status = ToolStatus::Failed;
        c.output = Some(msg.into());
    }
    Step::Call(c, 300)
}

fn write_file(cwd: &str, id: &str, path: &str, content: &str) -> Step {
    let mut c = call(
        id,
        "write_file",
        ToolKind::Edit,
        path,
        json!({ "path": path, "content": content }),
        ToolStatus::Completed,
        &format!("Created {cwd}/{path} ({} bytes)", content.len()),
    );
    c.diff = Some(FileDiff {
        path: path.into(),
        old: None,
        new: content.into(),
    });
    Step::Call(c, 300)
}

fn web_search(id: &str, q: &str) -> Step {
    let out = "1. i32::wrapping_add - Rust\n   https://doc.rust-lang.org/std/primitive.i32.html\n   Wrapping (modular) addition. Computes self + rhs, wrapping around at the boundary of the type.\n\n2. Integer overflow - The Rust Book\n   https://doc.rust-lang.org/book/ch03-02-data-types.html\n   When you're compiling in debug mode, Rust includes checks for integer overflow that cause your program to panic at runtime.";
    Step::Call(
        call(
            id,
            "web_search",
            ToolKind::Search,
            q,
            json!({ "query": q }),
            ToolStatus::Completed,
            out,
        ),
        400,
    )
}

fn web_fetch(id: &str, url: &str) -> Step {
    Step::Call(
        call(
            id,
            "web_fetch",
            ToolKind::Fetch,
            url,
            json!({ "url": url }),
            ToolStatus::Completed,
            "Primitive Type i32\n\nThe 32-bit signed integer type.",
        ),
        300,
    )
}

fn mcp(id: &str, tool: &str, args: Value, out: &str, ok: bool) -> Step {
    let name = format!("fake__{tool}");
    Step::Call(
        call(
            id,
            &name,
            ToolKind::Other,
            &name,
            args,
            if ok {
                ToolStatus::Completed
            } else {
                ToolStatus::Failed
            },
            out,
        ),
        300,
    )
}

fn todos(id: &str, items: &[(&str, TodoStatus)]) -> Vec<Step> {
    let list: Vec<Todo> = items
        .iter()
        .map(|(t, s)| Todo {
            text: (*t).into(),
            status: *s,
        })
        .collect();
    let json_items: Vec<Value> = list
        .iter()
        .map(|t| {
            let s = match t.status {
                TodoStatus::Pending => "pending",
                TodoStatus::InProgress => "in_progress",
                TodoStatus::Completed => "completed",
            };
            json!({ "content": t.text, "status": s })
        })
        .collect();
    let done = list
        .iter()
        .filter(|t| t.status == TodoStatus::Completed)
        .count();
    let mut out = format!("todo list updated: {done}/{} done", list.len());
    for t in &list {
        let g = match t.status {
            TodoStatus::Pending => '\u{2610}',
            TodoStatus::InProgress => '\u{25b8}',
            TodoStatus::Completed => '\u{2713}',
        };
        out.push_str(&format!("\n{g} {}", t.text));
    }
    vec![
        Step::Call(
            call(
                id,
                "todo",
                ToolKind::Other,
                "todo",
                json!({ "action": "write", "items": json_items }),
                ToolStatus::Completed,
                &out,
            ),
            150,
        ),
        Step::Todos(list),
    ]
}

fn think(s: &str) -> Step {
    Step::Think(s.into())
}

fn text(s: &str) -> Step {
    Step::Text(s.into())
}

const LONG_MD: &str = "## Summary\n\nI read `src/lib.rs`, changed `add` to **wrap on overflow**, and ran the tests. The _short_ version: nothing else moved.\n\n### What changed\n\n1. `add` now calls `wrapping_add`.\n2. A new `sub` uses `saturating_sub`, because a wrapping subtract hid a bug in `main.rs`.\n   - nested bullet with `inline code`\n   - another one with a [link to the docs](https://doc.rust-lang.org/std/primitive.i32.html)\n     that wraps onto a second line when the terminal is narrow enough to need it\n3. ~~Drop the old helper~~ kept, since `main.rs` still uses it.\n\n- [x] tests pass\n- [ ] benchmarks not run\n\n> Overflow in release builds wraps silently; in debug builds it panics. That difference is the whole reason for this change.\n\n```rust\npub fn add(a: i32, b: i32) -> i32 {\n    // wrap instead of panicking in debug builds\n    a.wrapping_add(b)\n}\n\n#[test]\nfn wraps() {\n    assert_eq!(add(i32::MAX, 1), i32::MIN);\n}\n```\n\n```python\ndef add(a: int, b: int) -> int:\n    return (a + b + 2**31) % 2**32 - 2**31\n```\n\n```json\n{\"name\": \"demo\", \"version\": \"0.1.0\", \"private\": true}\n```\n\n```diff\n-    a + b\n+    a.wrapping_add(b)\n```\n\n```\nplain fence with no language\n```\n\n| step | result | exit |\n|------|--------|------|\n| read | ok | 0 |\n| patch | ok | 0 |\n| cargo test | failed on purpose, with a long note that makes this cell wider than the others | 1 |\n\n---\n\nFinal paragraph with *emphasis*, **strong**, ***both***, and a very long unbroken-looking sentence that keeps going so the wrapper has to break it at a word boundary somewhere near the right edge of the terminal, whatever width that happens to be.\n";

fn long_lines() -> String {
    (1..=120)
        .map(|i| {
            format!("{i:>3}. line {i} of a long answer that is here only to scroll the terminal")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn permission(id: &str, c: &ToolCall, rule: &str) -> PermissionRequest {
    PermissionRequest {
        id: id.into(),
        tool: c.name.clone(),
        kind: c.kind,
        title: c.title.clone(),
        input: c.input.clone(),
        diff: c.diff.clone(),
        rule: rule.into(),
    }
}

fn steps(name: &str, cwd: &str) -> Option<Vec<Step>> {
    use TodoStatus::*;
    let ls = "total 28\ndrwxr-xr-x 4 user user 4096 Oct  5 12:00 .\ndrwxr-xr-x 3 user user 4096 Oct  5 12:00 ..\ndrwxr-xr-x 8 user user 4096 Oct  5 12:00 .git\n-rw-r--r-- 1 user user   57 Oct  5 12:00 Cargo.toml\n-rw-r--r-- 1 user user   46 Oct  5 12:00 README.md\n-rw-r--r-- 1 user user   14 Oct  5 12:00 notes.txt\ndrwxr-xr-x 2 user user 4096 Oct  5 12:00 src";
    let v = match name {
        "text" => vec![
            think("**Thinking about the greeting** The user said hello, so a short friendly reply is enough."),
            Step::Slow("Hello! I can read and edit files in this project, run commands, and explain what I find. What would you like to do?".into(), 20, 90),
        ],
        "markdown" => vec![
            think("**Preparing a long answer** I will cover lists, code, a table and a quote."),
            text(LONG_MD),
        ],
        "long" => vec![text(&long_lines())],
        "slow" => vec![
            Step::Sleep(6.0),
            think("**Planning a slow answer** This reply streams slowly so the working state can be captured."),
            Step::Slow("Streaming a long paragraph one small chunk at a time. ".repeat(18).trim().into(), 40, 150),
        ],
        "explore" => vec![
            think("**Exploring the repo** List files, read the library, search for callers."),
            Step::Call(call("x1", "list_files", ToolKind::Read, ".", json!({ "path": "." }), ToolStatus::Completed, "Cargo.toml\nREADME.md\nnotes.txt\nsrc/lib.rs\nsrc/main.rs"), 200),
            read_file("x2", "src/lib.rs", LIB_RS),
            Step::Call(call("x3", "search_files", ToolKind::Search, "add", json!({ "pattern": "add", "path": "src" }), ToolStatus::Completed, "src/lib.rs:1:pub fn add(a: i32, b: i32) -> i32 {\nsrc/main.rs:2:    println!(\"{}\", demo::add(1, 2));"), 200),
            text("`add` is only called from `main.rs`."),
        ],
        "explore-sh" => vec![
            think("**Exploring the repo** List files, read the library, search for callers."),
            exec("x1", "ls -la", ls, 0),
            exec("x2", "cat src/lib.rs", LIB_RS.trim_end(), 0),
            exec("x3", "rg -n add src", "src/lib.rs:1:pub fn add(a: i32, b: i32) -> i32 {\nsrc/main.rs:2:    println!(\"{}\", demo::add(1, 2));", 0),
            text("`add` is only called from `main.rs`."),
        ],
        "exec" => {
            let seq: Vec<String> = (1..=120).map(|i| i.to_string()).collect();
            vec![
                exec("e1", "echo hello from the shell", "hello from the shell", 0),
                exec("e2", "sh -c 'echo running 4 tests; echo \"test add ... ok\"; echo \"test sub ... FAILED\" >&2; exit 101'", "running 4 tests\ntest add ... ok\nstderr:\ntest sub ... FAILED", 101),
                exec("e3", "seq 1 120", &seq.join("\n"), 0),
                exec("e4", "git status --short && printf 'a very long line %.0s' $(seq 1 30); echo; git log --oneline | head -3", &format!("{}\n3f2a9c1 init", "a very long line ".repeat(30)), 0),
                text("Ran four commands: one ok, one failed with exit 101, one with long output, one with a long line."),
            ]
        }
        "exec-edge" => vec![
            exec("e1", "set -e\necho one\necho two\necho three\necho four", "one\ntwo\nthree\nfour", 0),
            exec("e2", "printf '\\033[31mred\\033[0m ok\\n'", "\u{1b}[31mred\u{1b}[0m ok", 0),
            exec("e3", "true", "", 0),
            exec("e4", "false", "", 1),
            Step::Sleep(2.0),
            exec("e5", "sleep 2; echo late", "late", 0),
            text("Done."),
        ],
        "exec-sleep" => vec![
            Step::Call(call("e1", "execute", ToolKind::Execute, "sleep 20", json!({ "command": "sleep 20" }), ToolStatus::Completed, "(command succeeded with no output)"), 20_000),
            text("Done."),
        ],
        "approval" => {
            let c = call("a1", "execute", ToolKind::Execute, "touch approved.txt && echo created", json!({ "command": "touch approved.txt && echo created" }), ToolStatus::Completed, "created");
            vec![
                think("**Creating a file** I need to write a file, which may need approval."),
                Step::Gate(Box::new(permission("perm-a1", &c, "touch *")), Box::new(c)),
                text("Created `approved.txt`."),
            ]
        }
        "approval-escalate" => {
            let cmd = "curl -sS https://example.com -o /dev/null -w '%{http_code}\\n'";
            let c = call("a1", "execute", ToolKind::Execute, cmd, json!({ "command": cmd, "justification": "Do you want to allow a network request to example.com to check the site is up?" }), ToolStatus::Completed, "200");
            vec![
                Step::Gate(Box::new(permission("perm-a1", &c, "curl -sS *")), Box::new(c)),
                text("The request ran."),
            ]
        }
        "patch" => vec![
            think("**Patching add** Wrapping avoids the debug-build panic."),
            edit_file(cwd, "p1", "src/lib.rs", "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}", "pub fn add(a: i32, b: i32) -> i32 {\n    a.wrapping_add(b)\n}", None),
            text("Patched `add` to use `wrapping_add`."),
        ],
        // The same edit, held for approval first: the diff shows above the prompt (spec C.8).
        "patch-approval" => {
            let Step::Call(c, _) = edit_file(
                cwd,
                "p1",
                "src/lib.rs",
                "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}",
                "pub fn add(a: i32, b: i32) -> i32 {\n    a.wrapping_add(b)\n}",
                None,
            ) else {
                unreachable!("edit_file builds a call")
            };
            vec![
                think("**Patching add** Wrapping avoids the debug-build panic."),
                Step::Gate(Box::new(permission("perm-p1", &c, "src/**")), Box::new(c)),
                text("Patched `add` to use `wrapping_add`."),
            ]
        }
        // An MCP call that asks first (the elicitation form), twice: `echo`, then `fail`.
        "mcp-approval" => {
            let ask = |id: &str, tool: &str, args: Value, out: &str, ok: bool| {
                let Step::Call(c, _) = mcp(id, tool, args, out, ok) else {
                    unreachable!("mcp builds a call")
                };
                let rule = format!("mcp__fake__{tool}");
                let mut req = permission(&format!("perm-{id}"), &c, &rule);
                req.tool = format!("mcp__fake__{tool}");
                Step::Gate(Box::new(req), Box::new(c))
            };
            vec![
                ask("m1", "echo", json!({ "message": "hello from the model", "times": 2 }), "hello from the model\nhello from the model", true),
                ask("m2", "fail", json!({}), "tool failed on purpose", false),
                text("The `fake` MCP server answered the echo and failed on purpose."),
            ]
        }
        "patch-multi" => vec![
            edit_file(
                cwd,
                "p1",
                "src/lib.rs",
                "    a + b\n}",
                "    a.checked_add(b).unwrap_or(i32::MAX)\n}\n\npub fn sub(a: i32, b: i32) -> i32 {\n    a.saturating_sub(b)\n}",
                None,
            ),
            write_file(
                cwd,
                "p2",
                "src/util.rs",
                "//! Small helpers.\n\npub fn clamp(x: i32, lo: i32, hi: i32) -> i32 {\n    x.max(lo).min(hi)\n}\n",
            ),
            edit_file(cwd, "p3", "README.md", "hello world", "hello, wrapped world", None),
            exec("p4", "rm notes.txt", "", 0),
            text("Updated two files, added one, deleted `notes.txt`."),
        ],
        "patch-add" => vec![
            write_file(cwd, "p1", "src/new.rs", "pub fn id(x: i32) -> i32 {\n    x\n}\n"),
            text("Added src/new.rs."),
        ],
        "patch-delete" => vec![exec("p1", "rm notes.txt", "", 0), text("Deleted notes.txt.")],
        "patch-rename" => vec![
            exec("p1", "mv src/lib.rs src/lib2.rs", "", 0),
            edit_file(cwd, "p2", "src/lib2.rs", "    a + b", "    a.wrapping_add(b)", None),
            text("Renamed and patched."),
        ],
        "patch-wrap" => vec![
            edit_file(
                cwd,
                "p1",
                "README.md",
                "# demo project\nhello world",
                &format!("# demo project\n{}\ntab\there\nhello world", "a very long added line that keeps going ".repeat(6).trim_end()),
                None,
            ),
            text("Wrapped."),
        ],
        "patch-fail" => vec![
            edit_file(cwd, "p1", "missing.rs", "x", "y", Some("File not found: missing.rs")),
            text("The patch failed."),
        ],
        "plan" => {
            let mut s = vec![];
            s.extend(todos("t1", &[("Read src/lib.rs", InProgress), ("Patch add to wrapping_add", Pending), ("Run the tests", Pending)]));
            s.push(exec("t2", "cat src/lib.rs", LIB_RS.trim_end(), 0));
            s.extend(todos("t3", &[("Read src/lib.rs", Completed), ("Patch add to wrapping_add", InProgress), ("Run the tests", Pending)]));
            s.extend(todos("t4", &[("Read src/lib.rs", Completed), ("Patch add to wrapping_add", Completed), ("Run the tests", Completed)]));
            s.push(text("All three steps are done."));
            s
        }
        "proposed-plan" => vec![Step::Slow("<proposed_plan>\n# Fix overflow\n- change add to wrapping_add\n- add a test\n</proposed_plan>".into(), 6, 80)],
        "search" => vec![
            web_search("s1", "rust i32 wrapping_add overflow behaviour"),
            web_fetch("s2", "https://doc.rust-lang.org/std/primitive.i32.html"),
            text("`wrapping_add` wraps around at the boundary of the type, as documented."),
        ],
        "search-slow" => vec![
            Step::Sleep(3.0),
            web_search("s1", "rust wrapping_add"),
            web_fetch("s2", "https://doc.rust-lang.org/std/primitive.i32.html"),
            text("Found it."),
        ],
        "mcp" => vec![
            mcp("m1", "echo", json!({ "message": "hello from the model", "times": 2 }), "hello from the model\nhello from the model", true),
            mcp("m2", "fail", json!({}), "tool failed on purpose", false),
            mcp("m3", "image", json!({}), "(image/png, 68 bytes)", true),
            text("The `fake` MCP server answered the echo, failed on purpose, and returned an image."),
        ],
        "mcp-long" => vec![
            mcp("m1", "echo", json!({ "message": "x".repeat(400), "times": 1 }), &"x".repeat(400), true),
            mcp("m2", "echo", json!({ "message": "{\"a\": 1, \"b\": [1, 2, 3]}", "times": 1 }), "{\"a\": 1, \"b\": [1, 2, 3]}", true),
            text("Done."),
        ],
        "mcp-slow" => vec![
            Step::Call(call("m1", "fake__sleep", ToolKind::Other, "fake__sleep", json!({}), ToolStatus::Completed, "slept"), 8_000),
            text("Done."),
        ],
        "ask" => {
            let input = json!({"questions": [{
                "question": "Which approach should I take for the overflow fix?", "header": "Approach", "multiSelect": false,
                "options": [
                    {"label": "wrapping_add (Recommended)", "description": "Wrap on overflow, no panic."},
                    {"label": "checked_add", "description": "Return an Option and let the caller decide."},
                    {"label": "saturating_add", "description": "Clamp at the type bounds."}]}]});
            let c = call("q1", "AskUserQuestion", ToolKind::Other, "Which approach should I take for the overflow fix?", input, ToolStatus::Completed, "");
            vec![Step::Ask(Box::new(permission("ask-1", &c, "")), Box::new(c)), text("Going with your choice.")]
        }
        "perm" => {
            let input = json!({"reason": "Need to write build output outside the workspace and reach the network.", "permissions": {"network": {"enabled": true}, "file_system": {"write": ["/tmp"]}}});
            let c = call("r1", "request_permissions", ToolKind::Other, "request_permissions", input, ToolStatus::Completed, "granted");
            vec![Step::Gate(Box::new(permission("perm-r1", &c, "")), Box::new(c)), text("Permissions granted, continuing.")]
        }
        "reason-body" => vec![
            Step::Sleep(3.0),
            think("**Reading the config**\n\nI will open lib.rs first and then compare it with main.rs.\n\nThe signature decides the fix."),
            Step::Slow("Done.".into(), 6, 120),
        ],
        "reason-two" => vec![think("**First step**\n\nbody of the first part"), think("\n\n**Second step**\n\nbody of the second part"), text("Done.")],
        "reason-variants" => vec![think("**Just a title**"), think("\n\nplain thought with no bold title"), text("Done.")],
        "worked" => vec![exec("e1", "echo hi", "hi", 0), Step::Sleep(65.0), text("done")],
        "compact-summary" => vec![text("Summary of the session so far: the user asked for `add` to wrap on overflow; `src/lib.rs` was patched; tests were not run.")],
        "structured" => vec![text("{\"findings\":[{\"title\":\"[P1] add wraps silently\",\"body\":\"wrapping_add hides overflow in release and debug.\",\"confidence_score\":0.8,\"priority\":1}],\"overall_correctness\":\"patch is incorrect\",\"overall_explanation\":\"One finding.\",\"overall_confidence_score\":0.7}")],
        "tour" => {
            let mut s = vec![think("**Planning the change** I will read lib.rs, patch it, then check the toolchain.")];
            s.extend(todos("t1", &[("Read src/lib.rs", InProgress), ("Patch add to wrapping_add", Pending), ("Run the checks", Pending)]));
            s.push(exec("e1", "ls -la", ls, 0));
            s.push(exec("e2", "cat src/lib.rs", LIB_RS.trim_end(), 0));
            s.push(edit_file(cwd, "p1", "src/lib.rs", "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}", "pub fn add(a: i32, b: i32) -> i32 {\n    a.wrapping_add(b)\n}", None));
            s.push(web_search("s1", "rust i32 wrapping_add overflow behaviour"));
            s.push(mcp("m1", "echo", json!({ "message": "hello from the model" }), "hello from the model", true));
            s.push(exec("e3", "sh -c 'echo running 2 tests; echo \"test add ... ok\"; echo \"test sub ... FAILED\" >&2; exit 101'", "running 2 tests\ntest add ... ok\nstderr:\ntest sub ... FAILED", 101));
            s.push(text(LONG_MD));
            s
        }
        "error-500" => vec![Step::Sleep(1.0), Step::Fail("We're currently experiencing high demand, which may cause temporary errors.".into())],
        "error-429" => vec![Step::Sleep(1.0), Step::Fail("You've hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), visit https://chatgpt.com/codex/settings/usage to purchase more credits or try again later.".into())],
        "error-failed" => vec![Step::Sleep(1.0), Step::Fail("Wizard ran out of room in the model's context window. Start a new thread or clear earlier history before retrying.".into())],
        "error-quota" => vec![Step::Sleep(1.0), Step::Fail("Quota exceeded. Check your plan and billing details.".into())],
        "error-400" => vec![Step::Sleep(1.0), Step::Fail("{\"error\": {\"message\": \"Invalid request (scripted): unsupported parameter\", \"type\": \"invalid_request_error\"}}".into())],
        "error-drop" => vec![
            Step::Slow("Partial answer that never completes".into(), 6, 40),
            Step::Sleep(3.0),
            Step::Fail("stream disconnected before completion: stream closed before response.completed".into()),
        ],
        "error-conn" => vec![
            Step::Sleep(3.0),
            Step::Fail("stream disconnected before completion: error sending request for url (http://127.0.0.1:19080/v1/responses)".into()),
        ],
        _ => return None,
    };
    Some(v)
}

// ---- playback ------------------------------------------------------------------------------

async fn pause(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

/// `s` in pieces of `n` characters, the pace tools/fake-llm streams its text at.
fn char_pieces(s: &str, n: usize) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    chars.chunks(n.max(1)).map(|c| c.iter().collect()).collect()
}

async fn stream_chars(ev: &Tx, s: &str, n: usize, ms: u64) {
    for piece in char_pieces(s, n) {
        let _ = ev.send(Event::TextDelta(piece));
        pause(ms).await;
    }
}

async fn stream(ev: &Tx, s: &str, chunks: usize, ms: u64) {
    let words: Vec<&str> = s.split_inclusive(' ').collect();
    let per = words.len().div_ceil(chunks.max(1)).max(1);
    for group in words.chunks(per) {
        let _ = ev.send(Event::TextDelta(group.concat()));
        pause(ms).await;
    }
}

/// Plays the scenario and says how the turn ended. Unknown names answer with a hint, so a typo
/// in a shot script shows up on screen.
async fn play(name: &str, cwd: &str, ev: &Tx, decisions: &Decisions) -> StopReason {
    let Some(list) = steps(name, cwd) else {
        stream(
            ev,
            &format!("I do not know the scenario `{name}`. Try `fake:tour`, `fake:text`, `fake:markdown`."),
            4,
            30,
        )
        .await;
        return StopReason::EndTurn;
    };
    // the scripted server waits a second before its first event
    if !matches!(list.first(), Some(Step::Sleep(_))) {
        pause(1000).await;
    }
    for step in list {
        match step {
            Step::Think(t) => {
                let _ = ev.send(Event::ThoughtDelta(t));
                pause(150).await;
            }
            Step::Text(t) => stream_chars(ev, &t, 6, 40).await,
            Step::Slow(t, chunks, ms) => stream(ev, &t, chunks, ms).await,
            Step::Call(c, ms) => {
                let _ = ev.send(Event::Tool(ToolCall {
                    status: ToolStatus::Running,
                    output: None,
                    ..c.clone()
                }));
                pause(ms).await;
                let _ = ev.send(Event::Tool(c));
            }
            Step::Todos(t) => {
                let _ = ev.send(Event::Todos(t));
            }
            Step::Sleep(s) => tokio::time::sleep(Duration::from_secs_f32(s)).await,
            Step::Gate(req, c) => {
                let running = ToolCall {
                    status: ToolStatus::Pending,
                    output: None,
                    ..(*c).clone()
                };
                let _ = ev.send(Event::Tool(running.clone()));
                let _ = ev.send(Event::Permission(*req));
                match decisions.lock().await.recv().await {
                    Some((_, true, _)) => {
                        let _ = ev.send(Event::Tool(ToolCall {
                            status: ToolStatus::Running,
                            output: None,
                            ..running
                        }));
                        pause(300).await;
                        let _ = ev.send(Event::Tool(*c));
                    }
                    Some((_, false, note)) => {
                        let _ = ev.send(Event::Tool(ToolCall {
                            status: ToolStatus::Failed,
                            output: Some(if note.is_empty() {
                                "The user denied this tool call.".into()
                            } else {
                                format!("The user denied this tool call and said: {note}")
                            }),
                            ..running
                        }));
                        return StopReason::Cancelled;
                    }
                    None => return StopReason::Cancelled,
                }
            }
            Step::Ask(req, c) => {
                let _ = ev.send(Event::Tool(ToolCall {
                    status: ToolStatus::Running,
                    output: None,
                    ..(*c).clone()
                }));
                let _ = ev.send(Event::Permission(*req));
                match decisions.lock().await.recv().await {
                    Some((_, true, answers)) => {
                        let said = answers
                            .lines()
                            .filter_map(|l| l.split_once('\t'))
                            .map(|(q, a)| format!("\"{q}\"=\"{a}\""))
                            .collect::<Vec<_>>()
                            .join(", ");
                        let _ = ev.send(Event::Tool(ToolCall {
                            output: Some(format!("Your questions have been answered: {said}.")),
                            ..*c
                        }));
                    }
                    _ => return StopReason::Cancelled,
                }
            }
            Step::Fail(msg) => {
                let _ = ev.send(Event::Notice {
                    level: NoticeLevel::Error,
                    text: msg,
                });
                return StopReason::Error;
            }
        }
    }
    StopReason::EndTurn
}

/// The whole turn of a scenario as a flat event list, with no timing and every approval granted:
/// `TurnStart`, the scripted events, `TurnEnd`. For tests that feed a headless app directly.
pub fn scenario_events(name: &str, cwd: &str) -> Option<Vec<Event>> {
    let mut out = vec![Event::TurnStart];
    let mut end = StopReason::EndTurn;
    for step in steps(name, cwd)? {
        match step {
            Step::Think(t) => out.push(Event::ThoughtDelta(t)),
            Step::Text(t) => out.extend(char_pieces(&t, 6).into_iter().map(Event::TextDelta)),
            Step::Slow(t, chunks, _) => chunk_events(&mut out, &t, chunks),
            Step::Call(c, _) => {
                out.push(Event::Tool(ToolCall {
                    status: ToolStatus::Running,
                    output: None,
                    ..c.clone()
                }));
                out.push(Event::Tool(c));
            }
            Step::Todos(t) => out.push(Event::Todos(t)),
            Step::Sleep(_) | Step::Ask(..) => {}
            Step::Gate(_, c) => {
                out.push(Event::Tool(ToolCall {
                    status: ToolStatus::Running,
                    output: None,
                    ..(*c).clone()
                }));
                out.push(Event::Tool(*c));
            }
            Step::Fail(msg) => {
                out.push(Event::Notice {
                    level: NoticeLevel::Error,
                    text: msg,
                });
                end = StopReason::Error;
                break;
            }
        }
    }
    out.push(Event::TurnEnd(end));
    Some(out)
}

fn chunk_events(out: &mut Vec<Event>, s: &str, chunks: usize) {
    let words: Vec<&str> = s.split_inclusive(' ').collect();
    let per = words.len().div_ceil(chunks.max(1)).max(1);
    for group in words.chunks(per) {
        out.push(Event::TextDelta(group.concat()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_come_from_the_prompt() {
        assert_eq!(scenario_name("go fake:tour"), Some("tour"));
        assert_eq!(scenario_name("fake:error-500 please"), Some("error-500"));
        assert_eq!(scenario_name("no scenario here"), None);
    }

    #[test]
    fn all_scenarios_build() {
        for n in [
            "text",
            "markdown",
            "long",
            "slow",
            "explore",
            "explore-sh",
            "exec",
            "exec-edge",
            "exec-sleep",
            "approval",
            "approval-escalate",
            "patch",
            "patch-approval",
            "mcp-approval",
            "patch-multi",
            "patch-add",
            "patch-delete",
            "patch-rename",
            "patch-wrap",
            "patch-fail",
            "plan",
            "search",
            "search-slow",
            "mcp",
            "mcp-long",
            "mcp-slow",
            "ask",
            "perm",
            "reason-body",
            "reason-two",
            "reason-variants",
            "proposed-plan",
            "worked",
            "tour",
            "compact-summary",
            "structured",
            "error-500",
            "error-429",
            "error-failed",
            "error-quota",
            "error-400",
            "error-drop",
            "error-conn",
        ] {
            assert!(steps(n, "/tmp/proj").is_some(), "{n}");
        }
        assert!(steps("nope", "/tmp").is_none());
    }

    #[tokio::test]
    async fn patch_emits_a_running_then_a_finished_edit_with_its_diff() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (_dtx, drx) = mpsc::unbounded_channel();
        let d: Decisions = Arc::new(Mutex::new(drx));
        let end = play("patch", "/tmp/proj", &tx, &d).await;
        assert_eq!(end, StopReason::EndTurn);
        drop(tx);
        let mut calls = Vec::new();
        while let Some(e) = rx.recv().await {
            if let Event::Tool(c) = e {
                calls.push(c);
            }
        }
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].status, ToolStatus::Running);
        assert_eq!(calls[1].status, ToolStatus::Completed);
        assert_eq!(calls[1].name, "edit_file");
        assert_eq!(
            calls[1].diff.as_ref().unwrap().new,
            "pub fn add(a: i32, b: i32) -> i32 {\n    a.wrapping_add(b)\n}"
        );
    }

    #[tokio::test]
    async fn errors_end_the_turn_in_error_after_a_notice() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (_dtx, drx) = mpsc::unbounded_channel();
        let d: Decisions = Arc::new(Mutex::new(drx));
        let end = play("error-400", "/tmp", &tx, &d).await;
        assert_eq!(end, StopReason::Error);
        assert!(matches!(
            rx.recv().await,
            Some(Event::Notice {
                level: NoticeLevel::Error,
                ..
            })
        ));
    }
}
