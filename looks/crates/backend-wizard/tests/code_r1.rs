//! Regression tests for docs/critics/openw-code-r1.md findings that live in the core.

use agent_core::{Event, NoticeLevel, Request};
use backend_wizard::core::{Action, Core};

fn ready() -> Core {
    let mut c = Core::new("/w".into(), None, "wizard".into());
    c.start();
    c.on_line(r#"{"jsonrpc":"2.0","id":1,"result":{"agentInfo":{"version":"1"}}}"#);
    c.on_line(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s1","configOptions":[]}}"#);
    c
}

fn notices(acts: &[Action]) -> Vec<(NoticeLevel, String)> {
    acts.iter()
        .filter_map(|a| match a {
            Action::Emit(Event::Notice { level, text }) => Some((*level, text.clone())),
            _ => None,
        })
        .collect()
}

/// Finding 15: Cancel dropped the queue, and the transcript went on showing the prompt with
/// nothing to say it would never be answered.
#[test]
fn cancel_says_how_many_queued_prompts_it_dropped() {
    let mut c = ready();
    c.on_request(Request::Prompt("A".into()));
    c.on_request(Request::Prompt("B".into()));
    c.on_request(Request::Prompt("C".into()));
    let acts = c.on_request(Request::Cancel);
    let n = notices(&acts);
    assert!(
        n.iter()
            .any(|(_, t)| t.contains('2') && t.contains("not sent")),
        "{n:?}"
    );
    let acts = c.on_line(r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"cancelled"}}"#);
    let sent = |a: &[Action], s: &str| {
        a.iter()
            .any(|a| matches!(a, Action::Send(v) if v.to_string().contains(s)))
    };
    assert!(!sent(&acts, "\"B\"") && !sent(&acts, "\"C\""));
    // nothing queued, nothing to report
    assert!(notices(&c.on_request(Request::Cancel)).is_empty());
}

/// Finding 16: paging stopped after 21 pages (1050 sessions) and said nothing.
#[test]
fn the_session_listing_follows_the_cursor_past_21_pages_and_says_when_it_stops() {
    fn page(page: usize, more: bool) -> String {
        let sessions: Vec<String> = (0..50)
            .map(|i| format!(r#"{{"sessionId":"p{page}s{i}","title":"t","cwd":"/w","updatedAt":"2026-01-01T00:00:00Z"}}"#))
            .collect();
        let cursor = if more {
            format!(r#","nextCursor":"c{page}""#)
        } else {
            String::new()
        };
        format!(
            r#"{{"jsonrpc":"2.0","id":{{ID}},"result":{{"sessions":[{}]{cursor}}}}}"#,
            sessions.join(",")
        )
    }
    let run = |pages: usize, cursor_at_end: bool| {
        let mut c = ready();
        c.on_request(Request::ListSessions);
        let mut got = None;
        let mut said = Vec::new();
        for (id, p) in (3u64..).zip(0..pages) {
            let line = page(p, p + 1 < pages || cursor_at_end).replace("{ID}", &id.to_string());
            let acts = c.on_line(&line);
            said.extend(notices(&acts));
            for a in acts {
                if let Action::Emit(Event::Sessions(v)) = a {
                    got = Some(v.len());
                }
            }
            if got.is_some() {
                break;
            }
        }
        (got, said)
    };
    // 60 pages, the server's cursor ends on its own: all 3000 arrive, no warning
    let (got, said) = run(60, false);
    assert_eq!(got, Some(3000));
    assert!(said.is_empty(), "{said:?}");
    // a server that never stops: the listing is cut, and the user is told
    let (got, said) = run(500, true);
    assert!(got.is_some_and(|n| n >= 3000), "{got:?}");
    assert!(
        said.iter()
            .any(|(l, t)| *l == NoticeLevel::Warn && t.contains("stopped")),
        "{said:?}"
    );
}
