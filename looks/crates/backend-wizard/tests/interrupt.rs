//! An interrupt must stop the running tool, and a turn that will not end must not leave the
//! frontend busy forever. Uses a fake `wizard` that ignores `session/cancel`, like a wizard whose
//! tool or provider stream does not return. Its own test binary: it sets `OPENW_WIZARD_BIN`.

use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use agent_core::*;
use backend_wizard::WizardBackend;
use tokio::time::timeout;

const FAKE: &str = r#"#!/bin/sh
if [ "$1" = --version ]; then echo 'wizard 1.2.3'; exit 0; fi
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*) echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{}}" ;;
    *'"session/new"'*|*'"session/load"'*)
      echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"sessionId\":\"s1\",\"configOptions\":[]}}" ;;
    *'"session/prompt"'*)
      echo '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"tool_call","toolCallId":"t1","title":"execute","kind":"execute","status":"in_progress","rawInput":{"command":"sleep 31; echo openw-interrupt-test"}}}}'
      # wizard runs a tool as `sh -c <command>` in a group of its own
      setsid sh -c 'sleep 31; echo openw-interrupt-test' &
      ;;
  esac
done
"#;

fn alive() -> bool {
    std::process::Command::new("pgrep")
        .args(["-f", "sleep 31; echo openw-interrupt-test"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

async fn next(h: &mut BackendHandle) -> Event {
    timeout(Duration::from_secs(10), h.rx.recv())
        .await
        .expect("no event in 10s")
        .expect("backend closed")
}

#[tokio::test]
async fn cancel_kills_the_tool_and_a_stuck_turn_restarts_wizard() {
    let dir = std::env::temp_dir().join(format!("openw-interrupt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("wizard");
    std::fs::write(&bin, FAKE).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::env::set_var("OPENW_WIZARD_BIN", &bin);
    std::env::set_var("OPENW_CANCEL_GRACE_MS", "2500");

    let mut h = WizardBackend::spawn(dir.clone(), None).unwrap();
    assert!(matches!(next(&mut h).await, Event::Ready { .. }));
    h.tx.send(Request::Prompt("run it".into())).unwrap();
    loop {
        if let Event::Tool(c) = next(&mut h).await {
            assert_eq!(c.status, ToolStatus::Running);
            break;
        }
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(alive(), "the fake tool did not start");

    let t0 = Instant::now();
    h.tx.send(Request::Cancel).unwrap();
    while alive() {
        assert!(
            t0.elapsed() < Duration::from_secs(4),
            "tool outlived the cancel"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The fake never ends the turn; once the grace is over wizard is started again on the same
    // session and the frontend hears about it.
    let mut saw = (false, false, false);
    while !(saw.0 && saw.1 && saw.2) {
        match next(&mut h).await {
            Event::History { session_id, .. } => {
                assert_eq!(session_id, "s1");
                saw.0 = true;
            }
            Event::Ready { .. } => saw.1 = true,
            Event::Notice { level, text } => {
                assert_eq!(level, NoticeLevel::Warn);
                assert!(text.contains("restarted"), "{text}");
                saw.2 = true;
            }
            _ => {}
        }
    }
    assert!(
        t0.elapsed() >= Duration::from_millis(2400),
        "restarted too early"
    );

    // The new wizard takes prompts again.
    h.tx.send(Request::Prompt("again".into())).unwrap();
    loop {
        if let Event::TurnStart = next(&mut h).await {
            break;
        }
    }
    h.tx.send(Request::Shutdown).unwrap();
    drop(h);
    // Nothing of the fake's tool is left behind.
    tokio::time::sleep(Duration::from_millis(1800)).await;
    let _ = std::process::Command::new("pkill")
        .args(["-f", "sleep 31; echo openw-interrupt-test"])
        .status();
    let _ = std::fs::remove_dir_all(&dir);
}
