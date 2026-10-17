//! Tests that start a process. `missing_binary_*`, `fatal_*` and `cancel_restarts_*` use no
//! model (the last one runs tests/fake_claude.py) and run by default; the rest drive the real
//! `claude` CLI with haiku and are `#[ignore]`:
//!
//!     cargo test -p backend-claude --test live -- --ignored --test-threads=1

use agent_core::*;
use backend_claude::{history, ClaudeBackend, ClaudeOptions};
use std::path::PathBuf;
use std::time::Duration;

struct Cwd(PathBuf);

impl Cwd {
    fn new(name: &str) -> Cwd {
        let p = std::env::temp_dir().join(format!("openc-test-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        Cwd(p.canonicalize().unwrap())
    }
}

impl Drop for Cwd {
    fn drop(&mut self) {
        for d in history::project_dirs(&self.0) {
            std::fs::remove_dir_all(d).ok();
        }
        std::fs::remove_dir_all(&self.0).ok();
    }
}

fn haiku(cwd: &Cwd) -> ClaudeOptions {
    let mut o = ClaudeOptions::new(cwd.0.clone());
    o.model = Some("haiku".into());
    o
}

async fn next(h: &mut BackendHandle, secs: u64) -> Event {
    tokio::time::timeout(Duration::from_secs(secs), h.rx.recv())
        .await
        .expect("timed out waiting for an event")
        .expect("backend closed")
}

async fn until(
    h: &mut BackendHandle,
    secs: u64,
    mut pred: impl FnMut(&Event) -> bool,
) -> Vec<Event> {
    let mut seen = Vec::new();
    loop {
        let e = next(h, secs).await;
        let done = pred(&e);
        seen.push(e);
        if done {
            return seen;
        }
    }
}

async fn ready(h: &mut BackendHandle) -> (String, Config) {
    loop {
        match next(h, 60).await {
            Event::Ready { session_id, config } => return (session_id, config),
            Event::Fatal(m) => panic!("fatal before ready: {m}"),
            _ => {}
        }
    }
}

async fn turn(h: &mut BackendHandle, prompt: &str) -> Vec<Event> {
    h.tx.send(Request::Prompt(prompt.into())).unwrap();
    until(h, 120, |e| matches!(e, Event::TurnEnd(_) | Event::Fatal(_))).await
}

async fn config_changed(h: &mut BackendHandle) -> Config {
    loop {
        match next(h, 15).await {
            Event::ConfigChanged(c) => return c,
            Event::Commands(_) => {}
            other => panic!("expected ConfigChanged, got {other:?}"),
        }
    }
}

async fn error_notice(h: &mut BackendHandle) {
    loop {
        match next(h, 15).await {
            Event::Notice {
                level: NoticeLevel::Error,
                ..
            } => return,
            Event::Commands(_) => {}
            other => panic!("expected an error notice, got {other:?}"),
        }
    }
}

fn text(evs: &[Event]) -> String {
    evs.iter()
        .filter_map(|e| {
            if let Event::TextDelta(t) = e {
                Some(t.as_str())
            } else {
                None
            }
        })
        .collect()
}

fn notices(evs: &[Event]) -> Vec<(NoticeLevel, String)> {
    evs.iter()
        .filter_map(|e| {
            if let Event::Notice { level, text } = e {
                Some((*level, text.clone()))
            } else {
                None
            }
        })
        .collect()
}

#[tokio::test]
async fn missing_binary_is_an_error_from_spawn() {
    let mut o = ClaudeOptions::new(std::env::temp_dir());
    o.bin = "/nonexistent/claude".into();
    let err = ClaudeBackend::spawn_with(o, None)
        .err()
        .expect("spawn must fail");
    assert!(err.to_string().contains("/nonexistent/claude"), "{err:#}");
}

#[tokio::test]
async fn fatal_when_the_process_dies_before_answering() {
    let mut o = ClaudeOptions::new(std::env::temp_dir());
    o.bin = "false".into();
    let mut h = ClaudeBackend::spawn_with(o, None).unwrap();
    let evs = until(&mut h, 20, |e| matches!(e, Event::Fatal(_))).await;
    assert!(
        matches!(evs.last(), Some(Event::Fatal(m)) if m.starts_with("claude exited")),
        "{evs:?}"
    );
    h.tx.send(Request::Shutdown).unwrap();
}

#[tokio::test]
#[ignore]
async fn ready_commands_and_a_streamed_answer() {
    let cwd = Cwd::new("plain");
    let mut h = ClaudeBackend::spawn_with(haiku(&cwd), None).unwrap();
    let (sid, cfg) = ready(&mut h).await;
    assert_eq!(sid.len(), 36);
    assert!(cfg.backend.starts_with("claude "), "{}", cfg.backend);
    assert_eq!(cfg.model, "haiku");
    assert_eq!(cfg.mode, "bypassPermissions");
    assert_eq!(cfg.cwd, cwd.0.to_string_lossy());
    assert!(
        cfg.models.iter().any(|m| m.id == "opus") && cfg.models.iter().any(|m| m.id == "haiku")
    );
    assert_eq!(
        cfg.modes,
        ["default", "acceptEdits", "plan", "bypassPermissions"]
    );
    assert!(cfg.efforts.is_empty(), "haiku has no effort levels");
    let Event::Commands(cmds) = next(&mut h, 10).await else {
        panic!("Commands should follow Ready")
    };
    assert!(cmds.iter().any(|c| c.name == "context") && cmds.iter().any(|c| c.name == "compact"));

    let evs = turn(&mut h, "Reply with exactly: pong").await;
    assert_eq!(evs[0], Event::TurnStart);
    assert!(text(&evs).to_lowercase().contains("pong"), "{evs:?}");
    assert_eq!(evs.last(), Some(&Event::TurnEnd(StopReason::EndTurn)));
    let u = evs
        .iter()
        .rev()
        .find_map(|e| {
            if let Event::Usage(u) = e {
                Some(u.clone())
            } else {
                None
            }
        })
        .unwrap();
    assert!(
        u.cost_usd.unwrap_or(0.0) > 0.0 && u.context_tokens > 0 && u.context_window == 200_000,
        "{u:?}"
    );

    // a local slash command: notice, no model call, same turn shape
    let evs = turn(&mut h, "/context").await;
    assert!(
        notices(&evs)
            .iter()
            .any(|(_, t)| t.contains("Context Usage")),
        "{evs:?}"
    );
    h.tx.send(Request::Shutdown).unwrap();
}

#[tokio::test]
#[ignore]
async fn interrupt_cancels_the_turn_and_the_process_takes_the_next_prompt() {
    let cwd = Cwd::new("int");
    let mut h = ClaudeBackend::spawn_with(haiku(&cwd), None).unwrap();
    ready(&mut h).await;
    h.tx.send(Request::Prompt(
        "Count from 1 to 400, one number per line, nothing else.".into(),
    ))
    .unwrap();
    until(&mut h, 60, |e| matches!(e, Event::TextDelta(_))).await;
    h.tx.send(Request::Cancel).unwrap();
    let evs = until(&mut h, 20, |e| matches!(e, Event::TurnEnd(_))).await;
    assert_eq!(
        evs.last(),
        Some(&Event::TurnEnd(StopReason::Cancelled)),
        "{evs:?}"
    );
    assert!(
        notices(&evs).is_empty(),
        "a cancel is not an error: {evs:?}"
    );

    let evs = turn(&mut h, "Reply with exactly: after").await;
    assert!(text(&evs).contains("after"), "{evs:?}");
    assert_eq!(evs.last(), Some(&Event::TurnEnd(StopReason::EndTurn)));
    h.tx.send(Request::Shutdown).unwrap();
}

#[tokio::test]
#[ignore]
async fn settings_changes_go_through_control_requests() {
    let cwd = Cwd::new("cfg");
    let mut h = ClaudeBackend::spawn_with(haiku(&cwd), None).unwrap();
    ready(&mut h).await;

    h.tx.send(Request::SetMode("plan".into())).unwrap();
    let c = config_changed(&mut h).await;
    assert_eq!(c.mode, "plan");
    h.tx.send(Request::SetMode("bogus".into())).unwrap();
    error_notice(&mut h).await;
    h.tx.send(Request::SetMode("bypassPermissions".into()))
        .unwrap();
    let c = config_changed(&mut h).await;
    assert_eq!(c.mode, "bypassPermissions");

    h.tx.send(Request::SetEffort("low".into())).unwrap();
    error_notice(&mut h).await;

    h.tx.send(Request::SetModel("sonnet".into())).unwrap();
    let c = config_changed(&mut h).await;
    assert_eq!(c.model, "sonnet");
    assert!(c.efforts.contains(&"high".to_string()));
    let Event::Usage(u) = next(&mut h, 10).await else {
        panic!("a window refresh should follow a model change")
    };
    assert!(u.context_window >= 200_000);
    h.tx.send(Request::SetEffort("low".into())).unwrap();
    let c = config_changed(&mut h).await;
    assert_eq!(c.effort, "low");

    h.tx.send(Request::SetModel("no-such-model".into()))
        .unwrap();
    error_notice(&mut h).await;
    h.tx.send(Request::Shutdown).unwrap();
}

#[tokio::test]
#[ignore]
async fn tools_edit_files_and_diffs_come_through() {
    let cwd = Cwd::new("tools");
    let mut h = ClaudeBackend::spawn_with(haiku(&cwd), None).unwrap();
    ready(&mut h).await;
    let evs = turn(&mut h, "Use the Write tool to create hello.txt containing exactly: hi there. Then use Bash to run `cat hello.txt`. Nothing else.").await;
    assert_eq!(
        std::fs::read_to_string(cwd.0.join("hello.txt"))
            .unwrap()
            .trim(),
        "hi there"
    );
    let write = evs
        .iter()
        .rev()
        .find_map(|e| {
            if let Event::Tool(t) = e {
                Some(t).filter(|t| t.name == "Write")
            } else {
                None
            }
        })
        .expect("Write call");
    assert_eq!(
        (write.kind, write.status, write.title.as_str()),
        (ToolKind::Edit, ToolStatus::Completed, "hello.txt")
    );
    assert_eq!(write.diff.as_ref().unwrap().old, None);
    let bash = evs
        .iter()
        .rev()
        .find_map(|e| {
            if let Event::Tool(t) = e {
                Some(t).filter(|t| t.name == "Bash")
            } else {
                None
            }
        })
        .expect("Bash call");
    assert_eq!(bash.status, ToolStatus::Completed);
    assert!(bash.output.as_deref().unwrap().contains("hi there"));
    h.tx.send(Request::Shutdown).unwrap();
}

#[tokio::test]
#[ignore]
async fn denied_tool_in_default_mode_is_a_failed_call_with_a_reason() {
    let cwd = Cwd::new("deny");
    let mut o = haiku(&cwd);
    o.mode = "default".into();
    let mut h = ClaudeBackend::spawn_with(o, None).unwrap();
    ready(&mut h).await;
    let evs = turn(
        &mut h,
        "Run `touch denied.txt` with the Bash tool. If it is refused, say so.",
    )
    .await;
    let t = evs
        .iter()
        .rev()
        .find_map(|e| {
            if let Event::Tool(t) = e {
                Some(t).filter(|t| t.name == "Bash")
            } else {
                None
            }
        })
        .expect("Bash call");
    assert_eq!(t.status, ToolStatus::Failed, "{evs:?}");
    let out = t.output.as_deref().unwrap();
    assert!(
        out.contains("denied") || out.contains("needs approval"),
        "{out}"
    );
    assert!(!cwd.0.join("denied.txt").exists());
    assert_eq!(evs.last(), Some(&Event::TurnEnd(StopReason::EndTurn)));
    h.tx.send(Request::Shutdown).unwrap();
}

#[tokio::test]
#[ignore]
async fn resume_replays_history_and_keeps_context_and_sessions_list_it() {
    let cwd = Cwd::new("resume");
    let mut h = ClaudeBackend::spawn_with(haiku(&cwd), None).unwrap();
    let (sid, _) = ready(&mut h).await;
    turn(&mut h, "Remember the word tangerine. Reply with just: ok").await;
    h.tx.send(Request::ListSessions).unwrap();
    let Event::Sessions(list) = until(&mut h, 10, |e| matches!(e, Event::Sessions(_)))
        .await
        .pop()
        .unwrap()
    else {
        unreachable!()
    };
    let me = list.iter().find(|s| s.id == sid).expect("session listed");
    assert!(me.title.contains("tangerine"), "{me:?}");
    h.tx.send(Request::Shutdown).unwrap();
    drop(h);
    tokio::time::sleep(Duration::from_secs(1)).await;

    let mut h = ClaudeBackend::spawn_with(haiku(&cwd), Some(sid.clone())).unwrap();
    let mut seen = until(&mut h, 60, |e| matches!(e, Event::History { .. })).await;
    let Event::History { session_id, items } = seen.pop().unwrap() else {
        unreachable!()
    };
    assert_eq!(session_id, sid);
    assert!(
        matches!(&items[0], HistoryItem::User(t) if t.contains("tangerine")),
        "{items:?}"
    );
    assert!(items
        .iter()
        .any(|i| matches!(i, HistoryItem::Assistant(t) if t.to_lowercase().contains("ok"))));
    let evs = turn(&mut h, "Which word did I ask you to remember? One word.").await;
    assert!(text(&evs).to_lowercase().contains("tangerine"), "{evs:?}");
    h.tx.send(Request::Shutdown).unwrap();
}

#[tokio::test]
#[ignore]
async fn new_and_load_session_swap_the_process() {
    let cwd = Cwd::new("swap");
    let mut h = ClaudeBackend::spawn_with(haiku(&cwd), None).unwrap();
    let (first, _) = ready(&mut h).await;
    turn(&mut h, "Reply with exactly: one").await;

    h.tx.send(Request::NewSession).unwrap();
    let evs = until(&mut h, 60, |e| matches!(e, Event::History { .. })).await;
    let second = evs
        .iter()
        .find_map(|e| {
            if let Event::Ready { session_id, .. } = e {
                Some(session_id.clone())
            } else {
                None
            }
        })
        .expect("Ready for the new session");
    assert_ne!(second, first);
    assert!(
        matches!(evs.last(), Some(Event::History { session_id, items }) if *session_id == second && items.is_empty())
    );
    let evs = turn(&mut h, "Reply with exactly: two").await;
    assert!(text(&evs).contains("two"), "{evs:?}");

    h.tx.send(Request::LoadSession(first.clone())).unwrap();
    let evs = until(&mut h, 60, |e| matches!(e, Event::History { .. })).await;
    let Some(Event::History { session_id, items }) = evs.last() else {
        unreachable!()
    };
    assert_eq!(*session_id, first);
    assert!(
        matches!(&items[0], HistoryItem::User(t) if t.contains("one")),
        "{items:?}"
    );
    h.tx.send(Request::LoadSession("not-a-session".into()))
        .unwrap();
    error_notice(&mut h).await;

    h.tx.send(Request::ListSessions).unwrap();
    let Event::Sessions(list) = until(&mut h, 10, |e| matches!(e, Event::Sessions(_)))
        .await
        .pop()
        .unwrap()
    else {
        unreachable!()
    };
    assert!(
        list.iter().any(|s| s.id == first) && list.iter().any(|s| s.id == second),
        "{list:?}"
    );
    h.tx.send(Request::Shutdown).unwrap();
}

#[tokio::test]
async fn cancel_restarts_a_cli_that_ignores_the_interrupt() {
    let cwd = Cwd::new("stuck");
    let log = cwd.0.join("launches.log");
    let mut o = ClaudeOptions::new(cwd.0.clone());
    o.bin = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake_claude.py").into();
    o.extra = vec!["--fake-log".into(), log.to_string_lossy().into_owned()];
    o.cancel_grace = Duration::from_millis(300);
    let mut h = ClaudeBackend::spawn_with(o, None).unwrap();
    let (sid, cfg) = ready(&mut h).await;
    assert_eq!(cfg.backend, "claude 9.9.9");

    h.tx.send(Request::Prompt("hello".into())).unwrap();
    until(&mut h, 10, |e| matches!(e, Event::TextDelta(_))).await;
    h.tx.send(Request::Cancel).unwrap();
    let evs = until(&mut h, 15, |e| matches!(e, Event::TurnEnd(_))).await;
    assert_eq!(
        evs.last(),
        Some(&Event::TurnEnd(StopReason::Cancelled)),
        "{evs:?}"
    );
    assert!(
        notices(&evs)
            .iter()
            .any(|(l, t)| *l == NoticeLevel::Warn && t.contains("restarting")),
        "{evs:?}"
    );

    // the replacement process answers, on the same session id
    let evs = turn(&mut h, "again").await;
    assert_eq!(
        evs.last(),
        Some(&Event::TurnEnd(StopReason::EndTurn)),
        "{evs:?}"
    );
    assert_eq!(text(&evs), "partial");
    let launches: Vec<String> = std::fs::read_to_string(&log)
        .unwrap()
        .split_whitespace()
        .map(str::to_string)
        .collect();
    assert_eq!(launches, vec![sid.clone(), sid]);
    h.tx.send(Request::Shutdown).unwrap();
}
