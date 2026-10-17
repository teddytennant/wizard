//! `!cmd` shell mode, round 1 finding 4: interruptible, streams, and never leaves its process
//! tree behind. One test function: it sets `OPENW_SHELL_TIMEOUT_MS`, which is process-wide.

use std::time::{Duration, Instant};

use agent_core::transcript::Part;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use openw::app::{App, AppOpts, Msg};
use tokio::sync::mpsc::UnboundedReceiver;

const NONE: KeyModifiers = KeyModifiers::NONE;

fn alive(marker: &str) -> bool {
    std::process::Command::new("pgrep")
        .args(["-f", marker])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The marker's processes are gone within three seconds (a loaded box reaps slowly).
async fn gone(marker: &str) -> bool {
    for _ in 0..30 {
        if !alive(marker) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

fn key(app: &mut App, code: KeyCode) {
    app.on_key(KeyEvent::new(code, NONE));
}

fn run(app: &mut App, cmd: &str) {
    for c in format!("!{cmd}").chars() {
        key(app, KeyCode::Char(c));
    }
    key(app, KeyCode::Enter);
}

fn output(app: &App) -> String {
    app.transcript
        .messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| match p {
            Part::Tool(c) => c.output.clone(),
            _ => None,
        })
        .next_back()
        .unwrap_or_default()
}

/// Feed messages to the app until the shell reports its end; returns how long that took.
async fn until_done(app: &mut App, rx: &mut UnboundedReceiver<Msg>, limit: Duration) -> Duration {
    let t0 = Instant::now();
    loop {
        let msg = tokio::time::timeout(limit, rx.recv())
            .await
            .expect("the shell never finished")
            .expect("channel closed");
        let done = matches!(msg, Msg::Shell { .. });
        app.update(msg);
        if done {
            return t0.elapsed();
        }
    }
}

#[tokio::test]
async fn shell_mode_streams_stops_on_esc_esc_and_on_timeout_and_leaves_nothing() {
    let (mtx, mut mrx) = tokio::sync::mpsc::unbounded_channel();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        AppOpts {
            cwd: std::env::temp_dir(),
            mock: true,
            seed: 2,
            ..Default::default()
        },
        tx,
        mtx,
    );
    app.size = (100, 30);
    app.frozen = Some(Duration::ZERO);

    // Output shows up while the command still runs.
    run(&mut app, "echo first; sleep 3; echo last");
    let mut seen = false;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(2) {
        if let Ok(Some(m)) = tokio::time::timeout(Duration::from_millis(200), mrx.recv()).await {
            app.update(m);
        }
        if output(&app).contains("first") {
            seen = true;
            break;
        }
    }
    assert!(seen, "no output before the command ended");
    assert!(!output(&app).contains("last"));

    // esc esc ends it, the sleep and the shell are gone, the row says so.
    key(&mut app, KeyCode::Esc);
    key(&mut app, KeyCode::Esc);
    let took = until_done(&mut app, &mut mrx, Duration::from_secs(10)).await;
    assert!(took < Duration::from_secs(2), "took {took:?}");
    assert!(output(&app).contains("interrupted"), "{}", output(&app));
    assert!(!app.transcript.busy);
    assert!(app.interrupt_at.is_none());

    // The tree: `sh -c` and its sleep, which the old code left behind as an orphan.
    run(&mut app, "sleep 7771; echo done");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(alive("sleep 7771"), "the command did not start");
    key(&mut app, KeyCode::Esc);
    key(&mut app, KeyCode::Esc);
    until_done(&mut app, &mut mrx, Duration::from_secs(10)).await;
    assert!(gone("sleep 7771").await, "sleep outlived the interrupt");

    // The time limit kills the group too.
    std::env::set_var("OPENW_SHELL_TIMEOUT_MS", "700");
    run(&mut app, "sleep 7772; echo done");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(alive("sleep 7772"));
    until_done(&mut app, &mut mrx, Duration::from_secs(10)).await;
    assert!(
        output(&app).contains("timed out after 700ms"),
        "{}",
        output(&app)
    );
    assert!(gone("sleep 7772").await, "sleep outlived the timeout");

    // Quitting with one running takes its tree along.
    std::env::remove_var("OPENW_SHELL_TIMEOUT_MS");
    run(&mut app, "sleep 7773; echo done");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(alive("sleep 7773"));
    app.kill_shell();
    assert!(gone("sleep 7773").await, "sleep outlived openw");
}
