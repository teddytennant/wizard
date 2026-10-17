//! "Degrade, never break": every screen at every small size, and markdown fed noise, must draw
//! without panicking and without writing outside the buffer.

mod common;

use agent_core::{Event, PermissionRequest, StopReason, ToolCall, ToolKind, ToolStatus};
use common::{hello, Harness, Mock};
use crossterm::event::KeyCode;

fn busy_session(h: &mut Harness, events: &[Event]) {
    h.ready();
    h.type_str("tools please");
    h.key(KeyCode::Enter);
    h.events(events.to_vec(), 30);
}

#[tokio::test]
async fn every_screen_survives_every_small_size() {
    let mut m = Mock::start();
    hello(&mut m).await;
    let tools = m.run("tools please").await;
    let long = m.run("long answer").await;
    let mut sizes: Vec<(u16, u16)> = Vec::new();
    for w in [1, 2, 3, 5, 8, 12, 20, 30, 39, 40, 41, 60, 80, 139, 140, 200] {
        for h in [1, 2, 3, 4, 5, 8, 11, 12, 13, 20, 29, 30, 31, 45] {
            sizes.push((w, h));
        }
    }
    for (w, h) in sizes {
        let mut hx = Harness::new(w, h);
        busy_session(&mut hx, &tools);
        hx.events(long.clone(), 20);
        hx.screen();
        // Overlays, popups, nav mode and a permission panel on top of the same transcript.
        hx.app.files = vec!["src/main.rs".into()];
        hx.type_str("/m");
        hx.screen();
        hx.key(KeyCode::Esc);
        hx.ctrl('u');
        for open in ['p', 't'] {
            hx.ctrl(open);
            hx.screen();
            hx.key(KeyCode::Esc);
        }
        hx.key(KeyCode::F(1));
        hx.screen();
        hx.key(KeyCode::Esc);
        hx.ctrl('\\');
        hx.screen();
        hx.key(KeyCode::Esc);
        hx.key(KeyCode::PageUp);
        hx.type_str("kkjo");
        hx.screen();
        hx.event(
            Event::Permission(PermissionRequest {
                id: "p".into(),
                tool: "Edit".into(),
                kind: ToolKind::Edit,
                title: "src/main.rs".into(),
                input: serde_json::json!({}),
                diff: Some(agent_core::FileDiff {
                    path: "/nonexistent/main.rs".into(),
                    old: Some("a\nb\n".into()),
                    new: "a\nc\nd\n".into(),
                }),
                rule: "src/**".into(),
            }),
            10,
        );
        hx.screen();
        hx.resize(w.saturating_add(7).min(250), h.saturating_add(3));
        hx.screen();
    }
}

#[test]
fn markdown_and_streaming_survive_noise() {
    use openc::palette::{Depth, Kind, Palette, UNICODE};
    use openc::ui::md::{render, MdState};
    use openc::ui::row::Cx;
    use std::time::Instant;
    let pal = Palette::new(Kind::Hearth, Depth::True);
    let theme = pal.theme();
    let alphabet: Vec<&str> = vec![
        "a",
        "bc",
        " ",
        " ",
        "\n",
        "\n\n",
        "*",
        "**",
        "`",
        "```",
        "```rust\n",
        "_",
        "#",
        "## ",
        ">",
        "> ",
        "-",
        "- ",
        "1. ",
        "|",
        "| a | b |\n|---|--:|\n",
        "[",
        "](",
        ")",
        "http://x.y",
        "日本語",
        "e\u{301}",
        "😀",
        "\t",
        "    ",
        "~~",
        "---\n",
        "<b>",
        "\u{7f}",
        "\x1b[31m",
        "&amp;",
        "![i](u)",
    ];
    let mut seed = 0x9e3779b97f4a7c15u64;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for _ in 0..400 {
        let n = (rnd() % 40) as usize + 1;
        let src: String = (0..n)
            .map(|_| alphabet[(rnd() % alphabet.len() as u64) as usize])
            .collect();
        let width = (rnd() % 90) as usize + 1;
        let cx = Cx {
            p: &pal,
            theme: &theme,
            g: &UNICODE,
            width,
            detail: rnd() % 2 == 0,
            now: Instant::now(),
            spin: 0,
        };
        let rows = render(&src, &cx);
        for r in &rows {
            // Below a few columns the indent alone is wider than the column; the painter clips.
            if width >= 30 {
                assert!(r.width() <= width + 2, "{src:?} at {width}: {:?}", r.text());
            }
        }
        let mut st = MdState::default();
        for end in (0..=src.len()).filter(|i| src.is_char_boundary(*i)) {
            st.update(&src[..end], &cx, false, end % 2 == 0);
        }
        st.update(&src, &cx, true, true);
    }
}

#[test]
fn smoke_finished_turn_with_every_tool_kind_at_a_tiny_size_does_not_panic() {
    let mut h = Harness::new(12, 6);
    h.ready();
    h.type_str("x");
    h.key(KeyCode::Enter);
    h.event(Event::TurnStart, 10);
    for (i, k) in [
        ToolKind::Read,
        ToolKind::Edit,
        ToolKind::Execute,
        ToolKind::Search,
        ToolKind::Fetch,
        ToolKind::Think,
        ToolKind::Other,
    ]
    .into_iter()
    .enumerate()
    {
        h.event(
            Event::Tool(ToolCall {
                id: format!("t{i}"),
                name: "Thing".into(),
                kind: k,
                title: "a/very/long/path/to/some/file.rs".into(),
                status: ToolStatus::Completed,
                output: Some("x\n".repeat(60)),
                ..Default::default()
            }),
            10,
        );
    }
    h.event(Event::TextDelta("done".into()), 10);
    h.event(Event::TurnEnd(StopReason::EndTurn), 10);
    h.key(KeyCode::Esc);
    h.type_str("kkkkkkkkoOjjy");
    h.screen();
}

#[test]
fn no_block_is_ever_wider_than_its_column_even_with_wide_text() {
    use openc::ui::row::Cx;
    use tuikit::width::display_width;
    for width in [30usize, 41, 57, 80, 100] {
        let mut h = Harness::new(width as u16 + 4, 30);
        h.ready();
        h.type_str("日本語のとても長い質問です、これは折り返される必要がありますよね 😀😀😀");
        h.key(KeyCode::Enter);
        h.event(Event::TurnStart, 10);
        h.event(
            Event::ThoughtDelta("考え中です。次に何をするか決めています".repeat(3)),
            10,
        );
        for (i, name) in ["Bash", "Read", "Edit", "Grep", "WebFetch"]
            .iter()
            .enumerate()
        {
            h.event(
                Event::Tool(ToolCall {
                    id: format!("t{i}"),
                    name: (*name).into(),
                    kind: [ToolKind::Execute, ToolKind::Read, ToolKind::Edit, ToolKind::Search, ToolKind::Fetch][i],
                    title: "ディレクトリ/とても長いファイル名/another_very_long_path_segment/ファイル.rs".into(),
                    input: serde_json::json!({"command": "echo 日本語日本語日本語日本語日本語日本語日本語日本語日本語 && ls"}),
                    status: if i == 2 { ToolStatus::Failed } else { ToolStatus::Completed },
                    output: Some("出力行 one two three four five six seven eight nine ten eleven twelve thirteen\nsecond".into()),
                    diff: (i == 2).then(|| agent_core::FileDiff { path: "/nonexistent/日本語.rs".into(), old: Some("一行目\n二行目\n".into()), new: "一行目\n三行目 with a long tail that must wrap because it is much longer than the column\n".into() }),
                    ..Default::default()
                }),
                10,
            );
        }
        h.event(Event::TextDelta("回答です。**太字**と`コード`と [リンク](http://example.com/日本語) を含みます。\n\n| 列一 | 列二 |\n|---|---|\n| 日本語日本語 | 😀 |\n\n```\n日本語のコード行がとても長くて折り返される必要がある場合のテスト用の行です\n```".into()), 10);
        h.event(
            Event::Notice {
                level: agent_core::NoticeLevel::Warn,
                text:
                    "警告: とても長い警告メッセージがここに入りますよ、折り返しに注意してください"
                        .into(),
            },
            10,
        );
        h.event(Event::TurnEnd(StopReason::EndTurn), 10);
        h.draw();
        let now = h.now();
        let c = h.app.geo.c as usize;
        let cx = Cx {
            p: &h.app.p,
            theme: &h.app.theme,
            g: &h.app.g,
            width: c,
            detail: true,
            now,
            spin: 0,
        };
        for line in h.app.tr.document(&cx) {
            assert!(
                display_width(&line) <= c,
                "width {c}: {line:?} is {} cells",
                display_width(&line)
            );
        }
    }
}
