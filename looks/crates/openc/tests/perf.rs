//! A 100,000-row transcript: first paint, paging, wheel scrolling, typing and resize, with
//! the numbers printed (`cargo test -p openc --test perf -- --nocapture`). The assertions are
//! loose in a debug build; set `OPENC_PERF_STRICT=1` with `--release` to hold the design's
//! budgets (key to paint p99 under 16 ms, page p95 under 8 ms, resize under 50 ms).

mod common;

use std::time::{Duration, Instant};

use agent_core::{Event, HistoryItem, ToolCall, ToolKind, ToolStatus};
use common::Harness;
use crossterm::event::KeyCode;

fn answer(i: usize) -> String {
    format!(
        "## Finding {i}\n\nThe parser reads a token and then peeks, so the off by one is in the lookahead of case {i}. \
         That also explains why `wrap` splits on single spaces and yields an empty token for a double space.\n\n\
         - step one of {i}\n- step two with `code` and **bold**\n- step three\n\n\
         ```rust\nfn lookahead(&mut self) -> Option<Token> {{\n    let t = self.peek()?;\n    self.pos += 1;\n    Some(t)\n}}\n```\n\n\
         | file | lines |\n|------|------:|\n| src/parser.rs | {i} |\n\nThat is the whole story for {i}."
    )
}

fn history(turns: usize) -> Vec<HistoryItem> {
    let mut v = Vec::new();
    for i in 0..turns {
        v.push(HistoryItem::User(format!(
            "question {i}: why does the parser misbehave on input {i}?"
        )));
        v.push(HistoryItem::Tool(ToolCall {
            id: format!("t{i}"),
            name: "Bash".into(),
            kind: ToolKind::Execute,
            title: "cargo test parser".into(),
            status: ToolStatus::Completed,
            output: Some(
                "running 5 tests\ntest a ... ok\ntest b ... ok\ntest c ... ok\ntest result: ok"
                    .into(),
            ),
            ..Default::default()
        }));
        v.push(HistoryItem::Assistant(answer(i)));
    }
    v
}

fn rss_mb() -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .map(str::to_string)
        })
        .and_then(|l| {
            l.split_whitespace()
                .nth(1)
                .and_then(|n| n.parse::<f64>().ok())
        })
        .map_or(0.0, |kb| kb / 1024.0)
}

fn pct(v: &mut [Duration], p: f64) -> Duration {
    v.sort();
    v[((v.len() as f64 * p) as usize).min(v.len() - 1)]
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

#[test]
fn hundred_thousand_rows_scroll_and_type_within_budget() {
    let strict = std::env::var_os("OPENC_PERF_STRICT").is_some();
    let (slack_key, slack_page, slack_resize) = if strict {
        (1.0, 1.0, 1.0)
    } else {
        (12.0, 12.0, 12.0)
    };
    let mut h = Harness::new(200, 60);
    h.ready();
    let items = history(4000);

    let t0 = Instant::now();
    h.event(
        Event::History {
            session_id: "big".into(),
            items,
        },
        1,
    );
    let replay = t0.elapsed();
    let t = Instant::now();
    h.draw();
    let first = t.elapsed();

    // Page up through the middle of the transcript.
    let mut pages = Vec::new();
    for _ in 0..200 {
        let t = Instant::now();
        h.key(KeyCode::PageUp);
        h.draw();
        pages.push(t.elapsed());
    }
    // Wheel at 120 events/s for a second, one frame per event.
    let mut wheel = Vec::new();
    for _ in 0..120 {
        let now = h.now();
        let t = Instant::now();
        h.app.on_mouse(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::ScrollDown,
                column: 5,
                row: 5,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            now,
        );
        h.advance(8);
        h.draw();
        wheel.push(t.elapsed());
    }
    // Typing with the transcript loaded and scrolled.
    let mut keys = Vec::new();
    for c in "hello world, typing into a long session".chars() {
        let t = Instant::now();
        h.key(KeyCode::Char(c));
        h.draw();
        keys.push(t.elapsed());
    }
    // Resize.
    let t = Instant::now();
    h.resize(120, 40);
    h.draw();
    let resize = t.elapsed();
    let t = Instant::now();
    h.resize(200, 60);
    h.draw();
    let resize_back = t.elapsed();
    // Jump to the end and back to the top.
    let t = Instant::now();
    h.key(KeyCode::Esc);
    h.key(KeyCode::Char('g'));
    h.draw();
    let top = t.elapsed();

    // Counting exactly lays out every block (about 12 s in a debug build) and would warm
    // every cache, so it is opt-in and done after the timings. 4000 turns of this shape are
    // 108,004 rows.
    let rows: usize = if std::env::var_os("OPENC_PERF_COUNT").is_some() {
        let now = h.now();
        let cx = openc::ui::row::Cx {
            p: &h.app.p,
            theme: &h.app.theme,
            g: &h.app.g,
            width: 100,
            detail: false,
            now,
            spin: 0,
        };
        let n = h.app.tr.document(&cx).len();
        assert!(n >= 100_000, "only {n} rows");
        n
    } else {
        let view = h.app.view.clone();
        h.app.tr.extent(&view, 60, 100).1
    };
    let (p95_page, p99_key, p95_wheel) = (
        pct(&mut pages.clone(), 0.95),
        pct(&mut keys.clone(), 0.99),
        pct(&mut wheel.clone(), 0.95),
    );
    println!("transcript rows            {rows} (estimate unless OPENC_PERF_COUNT=1)");
    println!("replay (History event)     {:.1} ms", ms(replay));
    println!("first paint at 200x60      {:.1} ms", ms(first));
    println!(
        "page up   p50/p95/max      {:.2} / {:.2} / {:.2} ms",
        ms(pct(&mut pages.clone(), 0.5)),
        ms(p95_page),
        ms(pct(&mut pages.clone(), 1.0))
    );
    println!(
        "wheel     p50/p95/max      {:.2} / {:.2} / {:.2} ms",
        ms(pct(&mut wheel.clone(), 0.5)),
        ms(p95_wheel),
        ms(pct(&mut wheel.clone(), 1.0))
    );
    println!(
        "key->paint p50/p99/max     {:.2} / {:.2} / {:.2} ms",
        ms(pct(&mut keys.clone(), 0.5)),
        ms(p99_key),
        ms(pct(&mut keys.clone(), 1.0))
    );
    println!(
        "resize 200x60 -> 120x40    {:.1} ms (back {:.1} ms)",
        ms(resize),
        ms(resize_back)
    );
    println!("jump to top                {:.1} ms", ms(top));
    println!("rss                        {:.0} MB", rss_mb());

    assert!(
        ms(p99_key) < 16.0 * slack_key,
        "key to paint p99 {:.2} ms",
        ms(p99_key)
    );
    assert!(
        ms(p95_page) < 8.0 * slack_page,
        "page p95 {:.2} ms",
        ms(p95_page)
    );
    assert!(
        ms(resize.max(resize_back)) < 50.0 * slack_resize,
        "resize {:.1} ms",
        ms(resize)
    );
    assert!(
        ms(first) < 50.0 * slack_resize,
        "first paint {:.1} ms",
        ms(first)
    );
}

/// Streaming: a long code block and a long answer, drawn every few deltas the way the loop does.
/// The cost of a frame must not grow with what has already arrived (the old tail re-render took
/// 70 ms a frame at 1000 code lines in a release build).
#[test]
fn streaming_frames_do_not_slow_down_as_the_answer_grows() {
    for (name, make) in [
        (
            "code fence",
            (|i: usize| format!("    let value_{i} = compute(a, b, {i}); // note\n"))
                as fn(usize) -> String,
        ),
        ("paragraphs", |i: usize| {
            format!("Paragraph {i} says that the parser peeks and then advances, with `code` and **bold**.\n\n")
        }),
    ] {
        let mut h = Harness::new(120, 40);
        h.ready();
        h.event(Event::TurnStart, 1);
        let mut head = if name == "code fence" {
            "Here it is.\n\n```rust\n".to_string()
        } else {
            String::new()
        };
        let mut frames: Vec<Duration> = Vec::new();
        let mut n = 0;
        for i in 0..700 {
            head.push_str(&make(i));
            let delta = std::mem::take(&mut head);
            // 60-byte deltas, a frame after every second one
            let b = delta.as_bytes();
            let mut at = 0;
            while at < b.len() {
                let mut end = (at + 60).min(b.len());
                while !delta.is_char_boundary(end) {
                    end += 1;
                }
                h.event(Event::TextDelta(delta[at..end].to_string()), 5);
                at = end;
                n += 1;
                if n % 2 == 0 {
                    let t = Instant::now();
                    h.draw();
                    frames.push(t.elapsed());
                }
            }
        }
        let len = frames.len();
        let avg = |s: &[Duration]| s.iter().sum::<Duration>() / s.len() as u32;
        let early = avg(&frames[len / 10..len / 5]);
        let late = avg(&frames[len - len / 10..]);
        println!("{name}: {len} frames, early {early:?}, late {late:?}");
        assert!(
            late < early * 5 + Duration::from_micros(500),
            "{name}: frames slowed from {early:?} to {late:?} as the answer grew"
        );
    }
}
