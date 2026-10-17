//! A stray non-UTF-8 byte on wizard's stdout must not end the session. Own test binary because
//! it sets `OPENW_WIZARD_BIN`.

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use agent_core::*;
use backend_wizard::WizardBackend;
use tokio::time::timeout;

const FAKE: &str = r#"#!/bin/sh
if [ "$1" = --version ]; then echo 'wizard 1.2.3'; exit 0; fi
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*) printf '\377\376 not utf-8, not json\n'; echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{}}" ;;
    *'"session/new"'*) echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"sessionId\":\"s1\",\"configOptions\":[]}}" ;;
    *'"session/prompt"'*)
      printf 'caf\351 on stdout\n'
      echo '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"still here"}}}}'
      echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"stopReason\":\"end_turn\"}}" ;;
  esac
done
"#;

async fn next(h: &mut BackendHandle) -> Event {
    timeout(Duration::from_secs(10), h.rx.recv())
        .await
        .expect("no event in 10s")
        .expect("backend closed")
}

#[tokio::test]
async fn invalid_utf8_lines_are_skipped_not_fatal() {
    let dir = std::env::temp_dir().join(format!("openw-lossy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("wizard");
    std::fs::write(&bin, FAKE).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::env::set_var("OPENW_WIZARD_BIN", &bin);

    let mut h = WizardBackend::spawn(dir.clone(), None).unwrap();
    assert!(matches!(next(&mut h).await, Event::Ready { .. }));
    h.tx.send(Request::Prompt("hi".into())).unwrap();
    let mut text = String::new();
    loop {
        match next(&mut h).await {
            Event::TextDelta(t) => text.push_str(&t),
            Event::TurnEnd(r) => {
                assert_eq!(r, StopReason::EndTurn);
                break;
            }
            Event::Fatal(m) => panic!("fatal: {m}"),
            _ => {}
        }
    }
    assert_eq!(text, "still here");
    let _ = std::fs::remove_dir_all(&dir);
}
