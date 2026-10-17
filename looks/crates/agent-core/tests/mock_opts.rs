//! The opt-in mock modes the fidelity replays rely on.

use agent_core::mock::{MockBackend, MockOpts};
use agent_core::{Event, Request};
use std::time::Duration;

async fn next(be: &mut agent_core::BackendHandle) -> Event {
    tokio::time::timeout(Duration::from_secs(10), be.rx.recv())
        .await
        .expect("mock stalled")
        .expect("mock closed")
}

#[tokio::test]
async fn a_fresh_mock_has_no_sessions_and_no_models_until_the_first_prompt() {
    let mut be = MockBackend::spawn_with(
        "/work/proj".into(),
        None,
        MockOpts {
            no_sessions: true,
            ..Default::default()
        },
    )
    .unwrap();
    let Event::Ready { config, .. } = next(&mut be).await else {
        panic!("Ready first")
    };
    assert!(config.models.is_empty());
    be.tx.send(Request::ListSessions).unwrap();
    loop {
        if let Event::Sessions(s) = next(&mut be).await {
            assert!(s.is_empty());
            break;
        }
    }
    be.tx.send(Request::Prompt("hello".into())).unwrap();
    let mut connected = false;
    let mut turn_over = false;
    while !(connected && turn_over) {
        match next(&mut be).await {
            Event::ConfigChanged(c) if !c.models.is_empty() => connected = true,
            Event::TurnEnd(_) => turn_over = true,
            _ => {}
        }
    }
    be.tx.send(Request::ListSessions).unwrap();
    loop {
        if let Event::Sessions(s) = next(&mut be).await {
            assert_eq!(s.len(), 1);
            assert_eq!(s[0].title, "Mock title");
            break;
        }
    }
}

#[tokio::test]
async fn echo_answers_with_the_prompt_text() {
    let mut be = MockBackend::spawn_with("/work/proj".into(), None, MockOpts::default()).unwrap();
    be.tx
        .send(Request::Prompt("[[echo]] ```rust\nfn x() {}\n```".into()))
        .unwrap();
    let mut text = String::new();
    loop {
        match next(&mut be).await {
            Event::TextDelta(t) => text.push_str(&t),
            Event::TurnEnd(_) => break,
            _ => {}
        }
    }
    assert_eq!(text, "```rust\nfn x() {}\n```");
}
