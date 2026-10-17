//! Talks to the real `wizard acp`. Run with:
//! `cargo test -p backend-wizard -- --ignored --test-threads=1`
//! Each test starts one wizard process at a time (the OAuth refresh token rotates)
//! and keeps model calls to a line or two.

use std::path::PathBuf;
use std::time::Duration;

use agent_core::*;
use backend_wizard::WizardBackend;
use tokio::time::timeout;

fn workdir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("openw-live-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), "a\n").unwrap();
    std::fs::write(dir.join("b.txt"), "b\n").unwrap();
    dir
}

async fn next(h: &mut BackendHandle, secs: u64) -> Event {
    timeout(Duration::from_secs(secs), h.rx.recv())
        .await
        .expect("timed out waiting for an event")
        .expect("backend closed")
}

/// Collect events until `pred` matches one, returning everything seen including it.
async fn until(h: &mut BackendHandle, secs: u64, pred: impl Fn(&Event) -> bool) -> Vec<Event> {
    let mut seen = Vec::new();
    loop {
        let e = next(h, secs).await;
        if let Event::Fatal(m) = &e {
            panic!("fatal: {m}\nseen: {seen:#?}");
        }
        let hit = pred(&e);
        seen.push(e);
        if hit {
            return seen;
        }
    }
}

async fn ready(h: &mut BackendHandle) -> (String, Config) {
    let evs = until(h, 30, |e| matches!(e, Event::Ready { .. })).await;
    match evs.into_iter().last() {
        Some(Event::Ready { session_id, config }) => (session_id, config),
        _ => unreachable!(),
    }
}

#[tokio::test]
#[ignore]
async fn handshake_commands_and_clean_shutdown() {
    let mut h = WizardBackend::spawn(workdir("hs"), None).unwrap();
    let (sid, config) = ready(&mut h).await;
    assert!(!sid.is_empty());
    assert!(config.backend.starts_with("wizard "), "{}", config.backend);
    assert!(!config.model.is_empty() && !config.models.is_empty());
    assert!(config.efforts.contains(&"high".to_string()));
    assert!(config.modes.contains(&"genie".to_string()));
    let evs = until(&mut h, 10, |e| matches!(e, Event::Commands(_))).await;
    let Some(Event::Commands(c)) = evs.last() else {
        unreachable!()
    };
    assert!(c.iter().any(|c| c.name == "status") && c.iter().any(|c| c.name == "model"));
    // /status is a command turn: one Notice, no model call, then Usage.
    h.tx.send(Request::Prompt("/status".into())).unwrap();
    let evs = until(&mut h, 20, |e| matches!(e, Event::Usage(_))).await;
    assert_eq!(evs[0], Event::TurnStart);
    assert!(
        matches!(&evs[1], Event::Notice { text, .. } if text.contains("model:")),
        "{evs:?}"
    );
    assert_eq!(evs[2], Event::TurnEnd(StopReason::EndTurn));
    // Options: effort and mode apply in place.
    h.tx.send(Request::SetEffort("low".into())).unwrap();
    let evs = until(&mut h, 10, |e| matches!(e, Event::ConfigChanged(_))).await;
    assert!(matches!(evs.last(), Some(Event::ConfigChanged(c)) if c.effort == "low"));
    h.tx.send(Request::SetMode("sovereign".into())).unwrap();
    let evs = until(&mut h, 10, |e| matches!(e, Event::ConfigChanged(_))).await;
    assert!(matches!(evs.last(), Some(Event::ConfigChanged(c)) if c.mode == "sovereign"));
    h.tx.send(Request::Shutdown).unwrap();
    // The channel closes once the process is gone.
    assert!(timeout(Duration::from_secs(10), async {
        while h.rx.recv().await.is_some() {}
    })
    .await
    .is_ok());
}

#[tokio::test]
#[ignore]
async fn model_switch_between_turns() {
    let mut h = WizardBackend::spawn(workdir("model"), None).unwrap();
    let (_, config) = ready(&mut h).await;
    let other = config
        .models
        .iter()
        .find(|m| m.id != config.model && m.provider == config.model.split('/').next().unwrap())
        .expect("a second model")
        .id
        .clone();
    h.tx.send(Request::SetModel(other.clone())).unwrap();
    let evs = until(&mut h, 20, |e| {
        matches!(e, Event::ConfigChanged(_) | Event::Notice { .. })
    })
    .await;
    assert!(
        matches!(evs.last(), Some(Event::ConfigChanged(c)) if c.model == other),
        "{evs:?}"
    );
    h.tx.send(Request::Shutdown).unwrap();
}

#[tokio::test]
#[ignore]
async fn tool_turn_list_load_and_resume() {
    let dir = workdir("tool");
    let mut h = WizardBackend::spawn(dir.clone(), None).unwrap();
    let (sid, _) = ready(&mut h).await;
    h.tx.send(Request::SetEffort("low".into())).unwrap();
    until(&mut h, 10, |e| matches!(e, Event::ConfigChanged(_))).await;

    h.tx.send(Request::Prompt(
        "Run `ls` with your shell tool and tell me how many files there are. Be brief.".into(),
    ))
    .unwrap();
    let evs = until(&mut h, 90, |e| matches!(e, Event::Usage(_))).await;
    println!("tool turn events:");
    for e in &evs {
        if !matches!(e, Event::TextDelta(_) | Event::ThoughtDelta(_)) {
            println!(
                "  {}",
                format!("{e:?}").chars().take(160).collect::<String>()
            );
        }
    }
    assert_eq!(evs[0], Event::TurnStart);
    let exec = evs
        .iter()
        .find_map(|e| match e {
            Event::Tool(t) if t.status == ToolStatus::Completed && t.kind == ToolKind::Execute => {
                Some(t)
            }
            _ => None,
        })
        .expect("an execute tool");
    assert!(exec.output.as_deref().unwrap_or("").contains("a.txt"));
    assert!(exec.title.contains("ls"));
    assert!(evs.iter().any(|e| matches!(e, Event::TextDelta(_))));
    assert!(evs.contains(&Event::TurnEnd(StopReason::EndTurn)));
    let Some(Event::Usage(u)) = evs.last() else {
        unreachable!()
    };
    assert!(u.input_tokens > 0 && u.output_tokens > 0);

    // The session is listed, then loaded back after switching away.
    h.tx.send(Request::ListSessions).unwrap();
    let evs = until(&mut h, 20, |e| matches!(e, Event::Sessions(_))).await;
    let Some(Event::Sessions(list)) = evs.last() else {
        unreachable!()
    };
    let me = list.iter().find(|s| s.id == sid).expect("session in list");
    assert!(me.title.contains("Run `ls`"));
    assert!(me.updated > 1_700_000_000);

    h.tx.send(Request::NewSession).unwrap();
    let (sid2, _) = ready(&mut h).await;
    assert_ne!(sid, sid2);
    h.tx.send(Request::LoadSession(sid.clone())).unwrap();
    let evs = until(&mut h, 20, |e| matches!(e, Event::Ready { .. })).await;
    let Some(Event::History { items, .. }) =
        evs.iter().find(|e| matches!(e, Event::History { .. }))
    else {
        panic!("{evs:?}")
    };
    assert!(matches!(&items[0], HistoryItem::User(t) if t.contains("Run `ls`")));
    assert!(items.iter().any(|i| matches!(i, HistoryItem::Tool(t) if t.name == "execute" && t.status == ToolStatus::Completed)));
    assert!(matches!(items.last(), Some(HistoryItem::Assistant(_))));
    h.tx.send(Request::Shutdown).unwrap();
    while h.rx.recv().await.is_some() {}

    // A second process resumes it at spawn.
    let mut h = WizardBackend::spawn(dir, Some(sid.clone())).unwrap();
    let evs = until(&mut h, 30, |e| matches!(e, Event::Ready { .. })).await;
    assert!(
        matches!(evs.first(), Some(Event::History { session_id, .. }) if *session_id == sid),
        "{evs:?}"
    );
    h.tx.send(Request::Shutdown).unwrap();
}

#[tokio::test]
#[ignore]
async fn cancel_ends_the_turn() {
    let mut h = WizardBackend::spawn(workdir("cancel"), None).unwrap();
    ready(&mut h).await;
    h.tx.send(Request::SetEffort("low".into())).unwrap();
    until(&mut h, 10, |e| matches!(e, Event::ConfigChanged(_))).await;
    h.tx.send(Request::Prompt(
        "Run `sleep 6` with your shell tool, then say done.".into(),
    ))
    .unwrap();
    until(
        &mut h,
        90,
        |e| matches!(e, Event::Tool(t) if t.status == ToolStatus::Running),
    )
    .await;
    h.tx.send(Request::SetEffort("high".into())).unwrap();
    h.tx.send(Request::Cancel).unwrap();
    // wizard cancels cooperatively: the running command finishes first.
    let evs = until(&mut h, 60, |e| matches!(e, Event::TurnEnd(_))).await;
    assert!(
        evs.iter().any(|e| matches!(
            e,
            Event::Notice {
                level: NoticeLevel::Warn,
                ..
            }
        )),
        "{evs:?}"
    );
    assert_eq!(evs.last(), Some(&Event::TurnEnd(StopReason::Cancelled)));
    h.tx.send(Request::Shutdown).unwrap();
}
