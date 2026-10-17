//! The `!cmd` runner (finding 5 and 6 of `docs/critics/piw-interaction-r1.md`): Esc stops the
//! whole tree, a limit stops a runaway, the output kept is bounded, quitting leaves nothing.

mod common;

use std::time::{Duration, Instant};

use common::*;
use crossterm::event::KeyCode;

/// Whether a process whose command line contains `needle` is alive.
fn alive(needle: &str) -> bool {
    std::process::Command::new("pgrep")
        .args(["-f", needle])
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false)
}

async fn gone_within(needle: &str, secs: u64) -> bool {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if !alive(needle) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

fn bang(h: &mut Harness, line: &str) {
    h.type_str(&format!("!{line}"));
    h.press(KeyCode::Enter);
}

async fn started(needle: &str) {
    for _ in 0..200 {
        if alive(needle) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("{needle} never started");
}

fn guard_exe() -> Option<std::path::PathBuf> {
    Some(std::path::PathBuf::from(env!("CARGO_BIN_EXE_piw")))
}

#[tokio::test]
async fn escape_stops_a_running_command_and_what_it_started() {
    for (tag, guard) in [("plain", None), ("guarded", guard_exe())] {
        let needle = format!("sleep 31.{}", if guard.is_some() { 41 } else { 42 });
        let mut h = Harness::new(100, 30);
        h.app.opts.cwd = std::env::temp_dir();
        h.app.opts.guard_exe = guard;
        // a subshell and a pipeline, so the sleep is not the shell's direct child
        bang(
            &mut h,
            &format!("(echo going; {needle}; echo notreached$((1+1))) | cat"),
        );
        started(&needle).await;
        h.press(KeyCode::Esc);
        assert!(h.text().contains("(cancelled)"), "{tag}: {}", h.text());
        assert!(
            gone_within(&needle, 20).await,
            "{tag}: `{needle}` is still running after Esc"
        );
        h.settle_bash().await;
        assert!(!h.text().contains("notreached2"));
    }
}

#[tokio::test]
async fn a_command_past_the_limit_is_stopped() {
    let mut h = Harness::new(100, 30);
    h.app.opts.cwd = std::env::temp_dir();
    h.app.opts.shell_limit = Some(Duration::from_millis(400));
    bang(&mut h, "echo before; sleep 31.43");
    h.settle_bash().await;
    let t = h.text();
    assert!(
        t.contains("before") && t.contains("(timed out after 400ms)"),
        "{t}"
    );
    assert!(gone_within("sleep 31.43", 20).await);
}

#[tokio::test]
async fn quitting_takes_a_running_command_along() {
    for (tag, guard) in [("plain", None), ("guarded", guard_exe())] {
        let needle = format!("sleep 31.{}", if guard.is_some() { 44 } else { 45 });
        let mut h = Harness::new(100, 30);
        h.app.opts.cwd = std::env::temp_dir();
        h.app.opts.guard_exe = guard;
        bang(&mut h, &format!("sh -c '{needle}'"));
        started(&needle).await;
        h.app.kill_bash();
        assert!(
            gone_within(&needle, 20).await,
            "{tag}: still running after quit"
        );
    }
}

#[tokio::test]
async fn output_is_kept_as_a_bounded_tail_and_the_command_can_still_be_stopped() {
    let mut h = Harness::new(100, 30);
    h.app.opts.cwd = std::env::temp_dir();
    // about 600 MB in the time this test lets it run; the old runner held all of it
    bang(&mut h, "yes 0123456789abcdef");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    h.deliver();
    let kept = h.app.bashes.last().unwrap().output.len();
    assert!(
        (1..=piw::shell::TAIL_BYTES + 200).contains(&kept),
        "{kept} bytes kept"
    );
    assert!(h
        .app
        .bashes
        .last()
        .unwrap()
        .output
        .starts_with("[earlier output not kept]"));
    h.press(KeyCode::Esc);
    assert!(gone_within("yes 0123456789abcdef", 20).await);
}

#[tokio::test]
async fn the_context_for_the_model_is_capped_and_cannot_close_its_own_fence() {
    let mut h = Harness::new(100, 30);
    h.app.opts.cwd = std::env::temp_dir();
    bang(
        &mut h,
        "printf '\\140\\140\\140\\nbreak out\\n\\140\\140\\140\\n'; head -c 200000 /dev/zero | tr '\\0' x",
    );
    h.settle_bash().await;
    h.type_str("next");
    h.press(KeyCode::Enter);
    let sent = h
        .sent()
        .into_iter()
        .find_map(|r| match r {
            agent_core::Request::Prompt(t) => Some(t),
            _ => None,
        })
        .unwrap();
    assert!(sent.len() < 60 * 1024, "{} bytes sent", sent.len());
    assert!(sent.ends_with("next"));
    // the opening and closing fence are the only ones
    assert_eq!(
        sent.matches("```").count(),
        2,
        "{}",
        &sent[..80.min(sent.len())]
    );
}

#[test]
fn output_is_cleaned_like_the_live_screen() {
    assert_eq!(
        piw::app::clean_output(
            "\x1b[31mred\x1b[0m\r\nok\t.\x1b]0;title\x07 end\x1b]52;c;QQ==\x1b\\\n"
        ),
        "red\nok   . end\n"
    );
}

#[tokio::test]
async fn a_cancelled_runs_late_result_does_not_stop_the_next_command() {
    let mut h = Harness::new(100, 30);
    h.app.opts.cwd = std::env::temp_dir();
    bang(&mut h, "sleep 31.46");
    started("sleep 31.46").await;
    h.press(KeyCode::Esc);
    bang(&mut h, "sleep 31.47");
    started("sleep 31.47").await;
    // the first run's End arrives now and is ignored
    for _ in 0..30 {
        h.deliver();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        alive("sleep 31.47"),
        "the second command was stopped by the first one's result"
    );
    assert!(h.app.bashes.last().unwrap().running);
    h.press(KeyCode::Esc);
    assert!(gone_within("sleep 31.47", 20).await);
}
