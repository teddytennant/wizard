use super::*;
use crate::agent::AgentEvent;
use crate::config::Config;
use crate::skin::Skin;

fn app() -> App {
    let mut app = App::new(Config::default());
    app.welcome_dismissed = true;
    app
}

/// The palette this skin ships with, pinned to this thread. The whole
/// suite writes the process-wide slot, so a test that swapped it would be
/// asserting against a value other threads are changing underneath it.
fn grok_theme() -> crate::theme::Pinned {
    crate::theme::pin(std::sync::Arc::new(
        crate::theme::load("grok").expect("the grok theme ships"),
    ))
}

#[test]
fn page_bg_is_grok_builds_bg_base() {
    let _theme = grok_theme();
    assert_eq!(page_bg(), Some(Color::Indexed(233)));
}

#[test]
fn empty_cells_are_the_page_not_the_terminal() {
    let _theme = grok_theme();
    let buf = render_buffer(&app(), 80, 24);
    assert_eq!(buf.cell((0, 0)).unwrap().bg, Color::Indexed(233));
    assert_eq!(buf.cell((40, 8)).unwrap().bg, Color::Indexed(233));
}

#[test]
fn the_user_prompt_arrow_is_accent_user_not_the_agent_rail() {
    let _theme = grok_theme();
    let entry = user_entry("hello", 40);
    let prefix = &entry.rows[0].line.spans[0];
    assert_eq!(prefix.content.as_ref(), PROMPT_ARROW);
    assert_eq!(prefix.style.fg, user_arrow_style(true).fg);
    assert_ne!(prefix.style.fg, theme::style(Token::Accent).fg);
    assert_eq!(user_arrow_style(true).fg, theme::style(Token::Muted).fg);
    assert_eq!(user_arrow_style(false).fg, theme::style(Token::Faint).fg);
}

#[test]
fn recede_area_dims_when_the_bg_is_not_rgb() {
    let mut buf = Buffer::empty(Rect::new(0, 0, 2, 1));
    buf.cell_mut((0, 0)).unwrap().modifier = Modifier::BOLD;
    recede_area(&mut buf, Rect::new(0, 0, 2, 1), Color::Reset);
    let cell = buf.cell((0, 0)).unwrap();
    assert!(cell.modifier.contains(Modifier::DIM));
    assert!(!cell.modifier.contains(Modifier::BOLD));
}

#[test]
fn recede_area_blends_rgb_fg_toward_rgb_bg() {
    let mut buf = Buffer::empty(Rect::new(0, 0, 1, 1));
    buf.cell_mut((0, 0))
        .unwrap()
        .set_fg(Color::Rgb(200, 200, 200));
    recede_area(&mut buf, Rect::new(0, 0, 1, 1), Color::Rgb(0, 0, 0));
    let cell = buf.cell((0, 0)).unwrap();
    assert_ne!(cell.fg, Color::Rgb(200, 200, 200));
    assert!(!cell.modifier.contains(Modifier::DIM));
}

#[test]
fn recede_area_blends_indexed_fg_toward_rgb_bg() {
    let mut buf = Buffer::empty(Rect::new(0, 0, 1, 1));
    buf.cell_mut((0, 0)).unwrap().set_fg(Color::Indexed(255));
    recede_area(&mut buf, Rect::new(0, 0, 1, 1), Color::Rgb(0, 0, 0));
    let cell = buf.cell((0, 0)).unwrap();
    assert!(!cell.modifier.contains(Modifier::DIM));
    assert!(
        matches!(cell.fg, Color::Indexed(_)),
        "Indexed recede stays in the 256 palette, got {:?}",
        cell.fg
    );
    assert_ne!(cell.fg, Color::Indexed(255));
}

/// Render at `width`×`height` under the `grok` skin, one string per row.
fn render(app: &App, width: u16, height: u16) -> Vec<String> {
    let buffer = render_buffer(app, width, height);
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect()
}

fn render_buffer(app: &App, width: u16, height: u16) -> ratatui::buffer::Buffer {
    let _pinned = crate::skin::pin(Skin::Grok);
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| super::super::draw(frame, app))
        .unwrap();
    terminal.backend().buffer().clone()
}

fn text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

// ── layout maths ────────────────────────────────────────────────────────

#[test]
fn the_block_columns_are_grok_builds_own_widths() {
    // │A│PL│content│PR│ = 1 + 2 + flex + 2. The doc comment upstream says
    // the right pad is 1; the config says 2, and the config is what runs.
    assert_eq!(CHROME_WIDTH, 5);
    // At 80 columns the screen keeps 2 either side plus a scrollbar column,
    // leaving a 75-wide block whose content is 70 columns.
    assert_eq!(content_width(76), 71);
    assert_eq!(bulleted_width(76), 69, "the ◆ takes two more");
    // Never zero, however narrow the terminal.
    assert_eq!(content_width(1), 1);
    assert_eq!(bulleted_width(1), 1);
}

#[test]
fn the_accent_rail_runs_the_blocks_whole_height_padding_included() {
    let entry = user_entry("hello", 40);
    let rows = decorate(&entry, 40, 0);
    // One padding row above, the prompt, one below.
    assert_eq!(rows.len(), 3);
    // The user prompt paints no rail of its own — the column is *cleared*,
    // which is a blank cell and not an absent one, so the prose still
    // starts three columns in.
    for row in &rows {
        let line = text(row);
        assert!(
            line.starts_with("   ") || line.trim().is_empty(),
            "the column is reserved: {line:?}"
        );
    }
    assert!(text(&rows[1]).starts_with("   \u{276f} hello"));
}

#[test]
fn a_running_tool_paints_a_bar_and_a_collapsed_one_drops_it() {
    let running = tool_entry(
        &ToolItem {
            name: "execute".into(),
            args: serde_json::json!({ "command": "cargo test" }),
            call_id: String::new(),
            output: None,
            progress: String::new(),
            timing: crate::transcript::ToolTiming::default(),
        },
        &app(),
        false,
        0,
        60,
    );
    assert!(text(&decorate(&running, 60, 0)[0]).starts_with(RAIL));

    let done = tool_entry(
        &ToolItem {
            name: "read_file".into(),
            args: serde_json::json!({ "path": "src/main.rs" }),
            call_id: String::new(),
            output: Some(crate::transcript::ToolItemOutput {
                content: "fn main() {}".into(),
                is_error: false,
            }),
            progress: String::new(),
            timing: crate::transcript::ToolTiming::default(),
        },
        &app(),
        true,
        0,
        60,
    );
    // Collapsed drops the rail (column stays). grok-build's renderer
    // clears `block.accent()` whenever Collapsed (`entry_renderer.rs:696-700`).
    assert_eq!(done.mode, Mode::Collapsed);
    let other = Entry {
        kind: Kind::Tool,
        mode: Mode::Collapsed,
        accent: Some(Token::Success),
        animated: false,
        pending: false,
        bullet: None,
        rows: vec![Row::plain(Line::raw("x"))],
        card: None,
        images: Vec::new(),
    };
    assert!(
        !text(&decorate(&other, 60, 0)[0]).starts_with(RAIL),
        "collapsed tools keep the column empty"
    );
}

#[test]
fn a_finished_subagent_drops_the_rail_a_running_one_keeps_it() {
    let running = subagent_entry(
        &ToolItem {
            name: "spawn_subagent".into(),
            args: serde_json::json!({ "subagent": "worker", "task": "look" }),
            call_id: String::new(),
            output: None,
            progress: String::new(),
            timing: crate::transcript::ToolTiming::default(),
        },
        &app(),
        0,
        60,
    );
    assert_eq!(running.accent, Some(Token::ToolRunning));
    assert!(text(&decorate(&running, 60, 0)[0]).starts_with(RAIL));

    let done = subagent_entry(
        &ToolItem {
            name: "spawn_subagent".into(),
            args: serde_json::json!({ "subagent": "worker", "task": "look" }),
            call_id: String::new(),
            output: Some(crate::transcript::ToolItemOutput {
                content: "ok".into(),
                is_error: false,
            }),
            progress: String::new(),
            timing: crate::transcript::ToolTiming::default(),
        },
        &app(),
        0,
        60,
    );
    assert!(done.accent.is_none());
    assert!(
        !text(&decorate(&done, 60, 0)[0]).starts_with(RAIL),
        "finished subagent keeps the column empty"
    );
    assert_eq!(done.bullet, Some((DIAMOND, Token::Success)));
}

#[test]
fn only_the_user_prompt_gets_vertical_padding_and_a_slab() {
    assert_eq!(Kind::UserPrompt.vpad(), 1);
    assert!(Kind::UserPrompt.tint().is_some());
    for kind in [
        Kind::Agent,
        Kind::Thinking,
        Kind::Tool,
        Kind::Subagent,
        Kind::BgTask,
        Kind::System,
    ] {
        assert_eq!(kind.vpad(), 0, "{kind:?} overrides vpad away");
        assert!(
            kind.tint().is_none(),
            "{kind:?} overrides its background away"
        );
    }
}

#[test]
fn the_prompt_band_covers_the_pads_and_the_padding_rows() {
    // The block background is the one thing a text dump cannot show, and
    // it is the whole of what a prompt band *is*: `bg_light` across the
    // accent column, both pads and the content, on every row of the entry
    // — the blank ones above and below included.
    let _theme = grok_theme();
    let rows = decorate(&user_entry("hello", 40), 40, 0);
    assert_eq!(rows.len(), 3, "one padding row each side of the prompt");
    let bg = Tint::Raised.resolve().expect("the grok theme declares one");
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(
            row.width(),
            40,
            "row {index} is a rectangle, not the shape of its text"
        );
        for span in &row.spans {
            assert_eq!(span.style.bg, Some(bg), "row {index}: {:?}", span.content);
        }
    }
}

#[test]
fn a_commands_output_sits_on_its_own_band_and_the_rest_of_the_block_does_not() {
    // Panel bands are per-*line*, not per-block: `bg_dark` behind the
    // output preview and nothing behind the header above it.
    let _theme = grok_theme();
    let entry = tool_entry(
        &ToolItem {
            name: "execute".into(),
            args: serde_json::json!({ "command": "cargo test" }),
            call_id: String::new(),
            output: Some(crate::transcript::ToolItemOutput {
                content: "ok".into(),
                is_error: false,
            }),
            progress: String::new(),
            timing: crate::transcript::ToolTiming::default(),
        },
        &app(),
        false,
        0,
        60,
    );
    let rows = decorate(&entry, 60, 0);
    let sunken = Tint::Sunken.resolve().expect("the grok theme declares one");
    assert!(
        rows[0].spans.iter().all(|span| span.style.bg.is_none()),
        "the header is unbanded: {:?}",
        text(&rows[0])
    );
    assert!(
        rows[1].spans.iter().all(|span| span.style.bg.is_none()),
        "the separator is unbanded: {:?}",
        text(&rows[1])
    );
    assert!(
        rows[2]
            .spans
            .iter()
            .any(|span| span.style.bg == Some(sunken)),
        "the output is banded: {:?}",
        text(&rows[2])
    );
    let last = rows[2].spans.last().expect("right pad");
    assert_eq!(last.style.bg, Some(sunken), "right pad sits on the panel");
    assert_eq!(
        rows[2].width() as u16,
        60,
        "panel row runs to the block edge"
    );
    // Rail and left pad stay off the panel.
    assert!(
        rows[2].spans[0].style.bg.is_none(),
        "the rail stays off the panel"
    );
}

// ── the grouping rule ───────────────────────────────────────────────────

#[test]
fn adjacent_collapsed_groupable_entries_pack_with_no_gap() {
    let collapsed_tool = || Entry {
        kind: Kind::Tool,
        mode: Mode::Collapsed,
        accent: Some(Token::Muted),
        animated: false,
        pending: false,
        bullet: None,
        rows: vec![Row::plain(Line::raw("x"))],
        card: None,
        images: Vec::new(),
    };
    let open_tool = || Entry {
        mode: Mode::Truncated,
        ..collapsed_tool()
    };
    let prose = || Entry {
        kind: Kind::Agent,
        mode: Mode::Collapsed,
        ..collapsed_tool()
    };

    assert_eq!(gap_after(&collapsed_tool(), &collapsed_tool()), 0);
    // One of them open: a gap row comes back.
    assert_eq!(gap_after(&collapsed_tool(), &open_tool()), 1);
    assert_eq!(gap_after(&open_tool(), &collapsed_tool()), 1);
    // An agent message is never groupable, whatever its mode says.
    assert_eq!(gap_after(&collapsed_tool(), &prose()), 1);
    assert_eq!(gap_after(&prose(), &prose()), 1);
    // Subagent and bg-task rows are groupable, and are always collapsed.
    let sub = Entry {
        kind: Kind::Subagent,
        ..collapsed_tool()
    };
    assert_eq!(gap_after(&sub, &collapsed_tool()), 0);
}

#[test]
fn a_run_of_collapsed_tools_is_solid_and_an_open_one_breaks_it() {
    let tool = |name: &str, folded: bool| Entry {
        kind: Kind::Tool,
        mode: if folded {
            Mode::Collapsed
        } else {
            Mode::Expanded
        },
        accent: Some(Token::Muted),
        animated: false,
        pending: false,
        bullet: Some((DIAMOND, Token::Muted)),
        rows: vec![Row::plain(Line::from(Span::raw(name.to_string())))],
        card: None,
        images: Vec::new(),
    };
    let entries = vec![tool("a", true), tool("b", true), tool("c", false)];
    let out = flatten(&entries, 40, 0, Vec::new());
    let rows: Vec<String> = out.lines.iter().map(text).collect();
    // a, b, gap, c — three entries and exactly one blank row.
    assert_eq!(rows.len(), 4, "{rows:?}");
    assert!(rows[2].trim().is_empty(), "{rows:?}");
}

// ── the composer's notch ────────────────────────────────────────────────

#[test]
fn the_info_line_is_a_notch_cut_into_the_bottom_border() {
    let mut app = app();
    app.status.model = "wizard-1".to_string();
    let rows = render(&app, 80, 24);
    // Find the bottom border of the composer: the row with `╰` and `╯`.
    let bottom = rows
        .iter()
        .find(|row| row.contains('\u{2570}') && row.contains('\u{256f}'))
        .expect("the composer has a bottom border");
    assert!(
        bottom.contains("wizard-1"),
        "the model is inlined: {bottom}"
    );
    assert!(bottom.contains("genie"), "and so is the mode: {bottom}");
    // The pads either side blank the `─` they sit on, so the label is
    // surrounded by spaces rather than by dashes.
    let at = bottom.find("wizard-1").unwrap();
    assert_eq!(
        &bottom[at - 1..at],
        " ",
        "a leading space blanks the border under it: {bottom}"
    );
    // And it really is the border row: corners at both ends.
    assert!(bottom.starts_with("  \u{2570}"), "{bottom}");
    assert!(bottom.trim_end().ends_with('\u{256f}'), "{bottom}");
}

#[test]
fn the_session_title_is_inlined_in_the_top_border() {
    let mut app = app();
    app.session_name = "fix the parser".to_string();
    let rows = render(&app, 80, 24);
    let top = rows
        .iter()
        .find(|row| row.contains('\u{256d}') && row.contains('\u{256e}'))
        .expect("the composer has a top border");
    assert!(top.contains("fix the parser"), "{top}");
    // Right-aligned, ending three cells before the ╮: one trailing pad
    // space (which blanks the `─` under it), then two plain border cells.
    // Counted in *cells*, not bytes — `─` is three bytes wide.
    let cells: Vec<char> = top.chars().collect();
    let corner = cells.iter().position(|ch| *ch == '\u{256e}').unwrap();
    let end = top
        .chars()
        .take(corner)
        .collect::<String>()
        .find("fix the parser")
        .is_some();
    assert!(end, "{top}");
    let tail: String = cells[corner - 3..=corner].iter().collect();
    assert_eq!(tail, " \u{2500}\u{2500}\u{256e}", "{top}");
}

#[test]
fn the_composer_budget_leaves_room_for_both_borders_and_the_prompt() {
    // 76 columns of box: two border/pad columns each side, two for `❯ `.
    assert_eq!(composer_budget(76), 70);
    assert!(composer_budget(4) >= 1, "never zero");
}

// ── wording and formatting ──────────────────────────────────────────────

#[test]
fn durations_and_token_counts_read_the_way_grok_build_prints_them() {
    assert_eq!(format_duration(Duration::from_millis(200)), "0.2s");
    assert_eq!(format_duration(Duration::from_secs(42)), "42s");
    assert_eq!(format_duration(Duration::from_secs(80)), "1m20s");
    assert_eq!(format_duration(Duration::from_secs(8000)), "2h13m");
    assert_eq!(format_tokens_short(999), "999");
    assert_eq!(format_tokens_short(1_234), "1.23k");
    assert_eq!(format_tokens_short(12_345), "12.3k");
    assert_eq!(format_tokens_short(500_000), "500k");
    assert_eq!(format_tokens_short(1_234_567), "1.23m");
}

#[test]
fn a_tool_header_reads_as_verb_operand_detail() {
    let read = ToolItem {
        name: "read_file".into(),
        args: serde_json::json!({ "path": "src/app/cli.rs", "start_line": 1, "end_line": 120 }),
        call_id: String::new(),
        output: None,
        progress: String::new(),
        timing: crate::transcript::ToolTiming::default(),
    };
    let header = text(&Line::from(tool_header(
        &read,
        ToolKind::Read,
        Mode::Collapsed,
        Path::new("/workspace"),
    )));
    assert_eq!(header, "Read cli.rs (1-120)");

    let search = ToolItem {
        name: "search_files".into(),
        args: serde_json::json!({ "pattern": "todo", "path": "src" }),
        call_id: String::new(),
        output: Some(crate::transcript::ToolItemOutput {
            content: "src/a.rs:1:todo\nsrc/a.rs:9:todo\nsrc/b.rs:2:todo".into(),
            is_error: false,
        }),
        progress: String::new(),
        timing: crate::transcript::ToolTiming::default(),
    };
    let header = text(&Line::from(tool_header(
        &search,
        ToolKind::Search,
        Mode::Collapsed,
        Path::new("/workspace"),
    )));
    // The pattern is Rust-debug-quoted, exactly as upstream prints it.
    assert_eq!(header, "Search \"todo\" in src (3 matches in 2 files)");

    let mcp = ToolItem {
        name: "linear__save_issue".into(),
        args: serde_json::json!({}),
        call_id: String::new(),
        output: None,
        progress: String::new(),
        timing: crate::transcript::ToolTiming::default(),
    };
    let header = text(&Line::from(tool_header(
        &mcp,
        ToolKind::Other,
        Mode::Truncated,
        Path::new("/workspace"),
    )));
    assert_eq!(header, "Linear Save Issue");

    let empty = ToolItem {
        name: "execute".into(),
        args: serde_json::json!({ "command": "  " }),
        call_id: String::new(),
        output: None,
        progress: String::new(),
        timing: crate::transcript::ToolTiming::default(),
    };
    let header = text(&Line::from(tool_header(
        &empty,
        ToolKind::Execute,
        Mode::Truncated,
        Path::new("/workspace"),
    )));
    assert_eq!(header, "Run \u{2026}");
}

#[test]
fn read_header_collapses_the_path_and_names_the_range() {
    let cwd = Path::new("/workspace");
    let mut read = ToolItem {
        name: "read_file".into(),
        args: serde_json::json!({
            "path": "/workspace/src/app/cli.rs",
            "start_line": 1,
            "end_line": 120
        }),
        call_id: String::new(),
        output: None,
        progress: String::new(),
        timing: crate::transcript::ToolTiming::default(),
    };
    let collapsed = text(&Line::from(tool_header(
        &read,
        ToolKind::Read,
        Mode::Collapsed,
        cwd,
    )));
    assert_eq!(collapsed, "Read cli.rs (1-120)");

    let expanded = text(&Line::from(tool_header(
        &read,
        ToolKind::Read,
        Mode::Truncated,
        cwd,
    )));
    assert_eq!(expanded, "Read src/app/cli.rs (1-120)");

    read.output = Some(crate::transcript::ToolItemOutput {
        content: "... [showing 2000 of 120 requested lines; total 400 lines — use start_line/end_line to read more]".into(),
        is_error: false,
    });
    let ranged = text(&Line::from(tool_header(
        &read,
        ToolKind::Read,
        Mode::Collapsed,
        cwd,
    )));
    assert_eq!(ranged, "Read cli.rs (1-120 of 400)");
}

#[test]
fn read_header_paints_the_path_as_link() {
    let cwd = Path::new("/workspace");
    let read = ToolItem {
        name: "read_file".into(),
        args: serde_json::json!({
            "path": "/workspace/src/app/cli.rs",
            "start_line": 1,
            "end_line": 120
        }),
        call_id: String::new(),
        output: None,
        progress: String::new(),
        timing: crate::transcript::ToolTiming::default(),
    };
    let collapsed = tool_header(&read, ToolKind::Read, Mode::Collapsed, cwd);
    assert_eq!(collapsed[0].style, theme::style(Token::Muted).bold());
    assert_eq!(collapsed[1].content.as_ref(), "cli.rs");
    assert_eq!(collapsed[1].style.fg, theme::style(Token::Muted).fg);

    let expanded = tool_header(&read, ToolKind::Read, Mode::Truncated, cwd);
    assert_eq!(expanded[1].content.as_ref(), "src/app/cli.rs");
    assert_eq!(expanded[1].style.fg, theme::style(Token::Link).fg);
}

#[test]
fn edit_and_listdir_headers_paint_the_path_as_link() {
    let cwd = Path::new("/workspace");
    let edit = ToolItem {
        name: "edit_file".into(),
        args: serde_json::json!({ "path": "src/ui/grok.rs" }),
        call_id: String::new(),
        output: None,
        progress: String::new(),
        timing: crate::transcript::ToolTiming::default(),
    };
    let collapsed = tool_header(&edit, ToolKind::Edit, Mode::Collapsed, cwd);
    assert_eq!(collapsed[1].content.as_ref(), "src/ui/grok.rs");
    assert_eq!(collapsed[1].style.fg, theme::style(Token::Muted).fg);
    let expanded = tool_header(&edit, ToolKind::Edit, Mode::Truncated, cwd);
    assert_eq!(expanded[1].style.fg, theme::style(Token::Link).fg);

    let list = ToolItem {
        name: "list_files".into(),
        args: serde_json::json!({ "path": "src" }),
        call_id: String::new(),
        output: None,
        progress: String::new(),
        timing: crate::transcript::ToolTiming::default(),
    };
    let collapsed = tool_header(&list, ToolKind::ListDir, Mode::Collapsed, cwd);
    assert_eq!(collapsed[1].style.fg, theme::style(Token::Muted).fg);
    let expanded = tool_header(&list, ToolKind::ListDir, Mode::Truncated, cwd);
    assert_eq!(expanded[1].content.as_ref(), "src");
    assert_eq!(expanded[1].style.fg, theme::style(Token::Link).fg);
}

#[test]
fn search_header_paints_accent_pattern_primary_in_and_link_path() {
    let cwd = Path::new("/workspace");
    let search = ToolItem {
        name: "search_files".into(),
        args: serde_json::json!({ "pattern": "todo", "path": "src" }),
        call_id: String::new(),
        output: None,
        progress: String::new(),
        timing: crate::transcript::ToolTiming::default(),
    };
    let collapsed = tool_header(&search, ToolKind::Search, Mode::Collapsed, cwd);
    assert_eq!(collapsed[0].content.as_ref(), "Search ");
    assert_eq!(collapsed[1].content.as_ref(), "\"todo\"");
    assert_eq!(collapsed[1].style.fg, theme::style(Token::Muted).fg);
    assert_eq!(collapsed[2].content.as_ref(), " in ");
    assert_eq!(collapsed[2].style.fg, theme::style(Token::Muted).fg);
    assert_eq!(collapsed[3].content.as_ref(), "src");
    assert_eq!(collapsed[3].style.fg, theme::style(Token::Muted).fg);

    let expanded = tool_header(&search, ToolKind::Search, Mode::Truncated, cwd);
    assert_eq!(expanded[1].content.as_ref(), "\"todo\"");
    assert_eq!(expanded[1].style.fg, theme::style(Token::Accent).fg);
    assert_eq!(expanded[2].content.as_ref(), " in ");
    assert_eq!(expanded[2].style.fg, theme::style(Token::Text).fg);
    assert_eq!(expanded[3].content.as_ref(), "src");
    assert_eq!(expanded[3].style.fg, theme::style(Token::Link).fg);
}

#[test]
fn search_header_paints_glob_as_term_when_pattern_is_trivial() {
    let cwd = Path::new("/workspace");
    let search = ToolItem {
        name: "search_files".into(),
        args: serde_json::json!({ "pattern": ".", "glob": "**/*.rs", "path": "src" }),
        call_id: String::new(),
        output: None,
        progress: String::new(),
        timing: crate::transcript::ToolTiming::default(),
    };
    let header = text(&Line::from(tool_header(
        &search,
        ToolKind::Search,
        Mode::Truncated,
        cwd,
    )));
    assert_eq!(header, "Search **/*.rs in src");

    let expanded = tool_header(&search, ToolKind::Search, Mode::Truncated, cwd);
    assert_eq!(expanded[1].content.as_ref(), "**/*.rs");
    assert_eq!(expanded[1].style.fg, theme::style(Token::Accent).fg);
    assert_eq!(expanded[2].content.as_ref(), " in ");
    assert_eq!(expanded[2].style.fg, theme::style(Token::Text).fg);
    assert_eq!(expanded[3].content.as_ref(), "src");
    assert_eq!(expanded[3].style.fg, theme::style(Token::Link).fg);
}

#[test]
fn search_header_paints_in_glob_before_path_when_pattern_is_real() {
    let cwd = Path::new("/workspace");
    let search = ToolItem {
        name: "search_files".into(),
        args: serde_json::json!({ "pattern": "todo", "glob": "**/*.rs", "path": "src" }),
        call_id: String::new(),
        output: None,
        progress: String::new(),
        timing: crate::transcript::ToolTiming::default(),
    };
    let header = text(&Line::from(tool_header(
        &search,
        ToolKind::Search,
        Mode::Truncated,
        cwd,
    )));
    assert_eq!(header, "Search \"todo\" in **/*.rs in src");

    let expanded = tool_header(&search, ToolKind::Search, Mode::Truncated, cwd);
    assert_eq!(expanded[1].content.as_ref(), "\"todo\"");
    assert_eq!(expanded[1].style.fg, theme::style(Token::Accent).fg);
    assert_eq!(expanded[2].content.as_ref(), " in ");
    assert_eq!(expanded[3].content.as_ref(), "**/*.rs");
    assert_eq!(expanded[3].style.fg, theme::style(Token::Accent).fg);
    assert_eq!(expanded[4].content.as_ref(), " in ");
    assert_eq!(expanded[5].content.as_ref(), "src");
    assert_eq!(expanded[5].style.fg, theme::style(Token::Link).fg);
}

#[test]
fn read_header_paints_skill_title_instead_of_path() {
    let cwd = Path::new("/workspace");
    let read = ToolItem {
        name: "read_file".into(),
        args: serde_json::json!({
            "path": "/home/user/.grok/skills/deploy/SKILL.md",
            "start_line": 1,
            "end_line": 80
        }),
        call_id: String::new(),
        output: Some(crate::transcript::ToolItemOutput {
            content: String::new(),
            is_error: false,
        }),
        progress: String::new(),
        timing: crate::transcript::ToolTiming::default(),
    };
    for mode in [Mode::Collapsed, Mode::Truncated] {
        let header = text(&Line::from(tool_header(&read, ToolKind::Read, mode, cwd)));
        assert_eq!(header, "Skill deploy", "{mode:?}");
    }

    let not_skill = ToolItem {
        name: "read_file".into(),
        args: serde_json::json!({ "path": "/x/skills/deploy/README.md" }),
        call_id: String::new(),
        output: None,
        progress: String::new(),
        timing: crate::transcript::ToolTiming::default(),
    };
    let header = text(&Line::from(tool_header(
        &not_skill,
        ToolKind::Read,
        Mode::Collapsed,
        cwd,
    )));
    assert_eq!(header, "Read README.md");
}

#[test]
fn read_header_paints_skill_title_as_link() {
    let cwd = Path::new("/workspace");
    let read = ToolItem {
        name: "read_file".into(),
        args: serde_json::json!({
            "path": "/home/user/.grok/skills/deploy/SKILL.md"
        }),
        call_id: String::new(),
        output: None,
        progress: String::new(),
        timing: crate::transcript::ToolTiming::default(),
    };
    let collapsed = tool_header(&read, ToolKind::Read, Mode::Collapsed, cwd);
    assert_eq!(collapsed[1].content.as_ref(), "deploy");
    assert_eq!(collapsed[1].style.fg, theme::style(Token::Muted).fg);

    let expanded = tool_header(&read, ToolKind::Read, Mode::Truncated, cwd);
    assert_eq!(expanded[1].content.as_ref(), "deploy");
    assert_eq!(expanded[1].style.fg, theme::style(Token::Link).fg);
}

#[test]
fn mcp_header_paints_bold_server_and_command_action() {
    let cwd = Path::new("/workspace");
    let mcp = ToolItem {
        name: "linear__save_issue".into(),
        args: serde_json::json!({}),
        call_id: String::new(),
        output: None,
        progress: String::new(),
        timing: crate::transcript::ToolTiming::default(),
    };
    let collapsed = tool_header(&mcp, ToolKind::Other, Mode::Collapsed, cwd);
    assert_eq!(collapsed[0].content.as_ref(), "Linear ");
    assert_eq!(collapsed[1].content.as_ref(), "Save Issue");
    assert_eq!(collapsed[0].style, theme::style(Token::Muted).bold());
    assert_eq!(collapsed[1].style, theme::style(Token::Muted));

    let expanded = tool_header(&mcp, ToolKind::Other, Mode::Truncated, cwd);
    assert_eq!(expanded[0].content.as_ref(), "Linear ");
    assert_eq!(expanded[1].content.as_ref(), "Save Issue");
    assert_eq!(expanded[0].style, theme::style(Token::Text).bold());
    assert_eq!(expanded[1].style, theme::style(Token::Code));
}

#[test]
fn read_header_appends_empty_image_and_pdf_suffix() {
    let cwd = Path::new("/workspace");
    let header = |path: &str, output: Option<&str>| {
        let read = ToolItem {
            name: "read_file".into(),
            args: serde_json::json!({ "path": path }),
            call_id: String::new(),
            output: output.map(|c| crate::transcript::ToolItemOutput {
                content: c.into(),
                is_error: false,
            }),
            progress: String::new(),
            timing: crate::transcript::ToolTiming::default(),
        };
        text(&Line::from(tool_header(
            &read,
            ToolKind::Read,
            Mode::Collapsed,
            cwd,
        )))
    };
    assert_eq!(header("notes.txt", Some("")), "Read notes.txt (empty)");
    assert_eq!(
        header("notes.txt", Some("(empty file)")),
        "Read notes.txt (empty)"
    );
    assert_eq!(header("notes.txt", None), "Read notes.txt");
    assert_eq!(
        header("shot.png", Some("shot.png: 8x8 PNG, 32 bytes")),
        "Read shot.png (image)"
    );
    assert_eq!(header("shot.png", None), "Read shot.png");
    assert_eq!(
        header("doc.pdf", Some("/Type /Pages /Type /Page /Type /Page")),
        "Read doc.pdf (2 pages)"
    );
    assert_eq!(header("doc.pdf", Some("%PDF-1.4")), "Read doc.pdf");
    assert_eq!(header("doc.pdf", None), "Read doc.pdf");

    let ranged_empty = ToolItem {
        name: "read_file".into(),
        args: serde_json::json!({
            "path": "notes.txt",
            "start_line": 1,
            "end_line": 10
        }),
        call_id: String::new(),
        output: Some(crate::transcript::ToolItemOutput {
            content: "(empty file)".into(),
            is_error: false,
        }),
        progress: String::new(),
        timing: crate::transcript::ToolTiming::default(),
    };
    let header = text(&Line::from(tool_header(
        &ranged_empty,
        ToolKind::Read,
        Mode::Collapsed,
        cwd,
    )));
    assert_eq!(header, "Read notes.txt (1-10) (empty)");
}

#[test]
fn execute_header_hang_wraps_under_run() {
    let command = "echo one two three four five six seven eight nine ten";
    let tool = ToolItem {
        name: "execute".into(),
        args: serde_json::json!({ "command": command }),
        call_id: String::new(),
        output: None,
        progress: String::new(),
        timing: crate::transcript::ToolTiming::default(),
    };
    let entry = tool_entry(&tool, &app(), false, 0, 24);
    let headers: Vec<String> = entry
        .rows
        .iter()
        .map(|row| text(&row.line))
        .take_while(|s| !s.is_empty())
        .collect();
    assert!(
        headers[0].starts_with("Run "),
        "first hang line starts with Run, got {headers:?}"
    );
    assert!(
        headers.len() > 1,
        "a command longer than the card must hang-wrap, got {headers:?}"
    );
    assert!(
        headers[1].starts_with("    "),
        "continuation hangs under Run, got {headers:?}"
    );
    let joined = headers.concat();
    assert!(
        joined.contains("echo") && joined.contains("ten"),
        "hang wrap must keep the whole command, got {headers:?}"
    );
}

#[test]
fn execute_header_soft_wraps_at_bash_operators() {
    let _theme = grok_theme();
    let command = "git status --short --branch && cargo test --workspace --all-features";
    let tool = ToolItem {
        name: "execute".into(),
        args: serde_json::json!({ "command": command }),
        call_id: String::new(),
        output: None,
        progress: String::new(),
        timing: crate::transcript::ToolTiming::default(),
    };
    let rows = execute_header_rows(&tool, Mode::Truncated, 40);
    let texts: Vec<String> = rows.iter().map(|row| text(&row.line)).collect();
    assert!(
        texts.len() >= 2,
        "operator wrap needs more than one row at width 40: {texts:?}"
    );
    assert!(
        texts[0].starts_with("Run "),
        "first row keeps the verb: {texts:?}"
    );
    assert!(
        texts[0].contains("&&"),
        "first row keeps the operator: {texts:?}"
    );
    assert!(
        !texts[0].contains("cargo"),
        "first row does not pack past &&: {texts:?}"
    );
    assert!(
        texts[1].starts_with("    cargo"),
        "continuation hangs under Run and starts at the next command: {texts:?}"
    );
    assert_eq!(texts.join(" ").split_whitespace().collect::<Vec<_>>(), {
        let mut expected = vec!["Run"];
        expected.extend(command.split_whitespace());
        expected
    });
}

#[test]
fn execute_header_does_not_wrap_inside_quoted_and() {
    let _theme = grok_theme();
    let command = r#"echo "keep && together" && echo next"#;
    let breaks = soft_break_offsets_after_operators(command);
    assert_eq!(breaks.len(), 1, "breaks={breaks:?}");
    let width = 28;
    assert!(UnicodeWidthStr::width(command) > width);
    let rows = soft_wrap_row_texts(command, 0, &breaks, width);
    let first = rows[0];
    assert!(
        first.contains(r#""keep && together""#),
        "quoted && must stay on the first row: {first:?}"
    );
    assert!(
        first.contains("&&"),
        "real operator stays with first row: {first:?}"
    );
    let header = execute_header_spans(
        command,
        theme::style(Token::Text).bold(),
        false,
        theme::style(Token::Code),
        width + "Run ".len(),
    );
    let texts: Vec<String> = header.iter().map(text).collect();
    assert!(
        texts[0].contains(r#""keep && together""#),
        "expanded Run header must not wrap inside quotes: {texts:?}"
    );
}

#[test]
fn execute_header_keeps_physical_command_lines() {
    let _theme = grok_theme();
    let tool = ToolItem {
        name: "execute".into(),
        args: serde_json::json!({ "command": "echo one\necho two" }),
        call_id: String::new(),
        output: None,
        progress: String::new(),
        timing: crate::transcript::ToolTiming::default(),
    };
    let rows = execute_header_rows(&tool, Mode::Truncated, 80);
    let texts: Vec<String> = rows.iter().map(|row| text(&row.line)).collect();
    assert_eq!(
        texts,
        vec!["Run echo one".to_string(), "    echo two".to_string()],
        "physical newlines stay rows instead of flattening onto one hang-wrap"
    );
}

#[test]
fn a_failed_tool_keeps_a_glyph_and_not_only_a_hue() {
    // The house rule that overrides fidelity: upstream turns the diamond
    // red and changes nothing else, which says nothing at 16 colours.
    // The glyphs come off the skin table, so pin the skin this file draws.
    let _pinned = crate::skin::pin(Skin::Grok);
    let (glyph, _) = tool_bullet(false, true);
    assert_ne!(glyph, DIAMOND);
    assert_eq!(tool_bullet(false, false).0, DIAMOND);
    assert_eq!(tool_bullet(true, false).0, DIAMOND);
}

#[test]
fn a_commands_output_is_windowed_with_a_count_and_a_files_is_not() {
    let _theme = grok_theme();
    let command = ToolItem {
        name: "execute".into(),
        args: serde_json::json!({ "command": "cargo test" }),
        call_id: String::new(),
        output: Some(crate::transcript::ToolItemOutput {
            content: (1..=12)
                .map(|n| format!("line {n}"))
                .collect::<Vec<_>>()
                .join("\n"),
            is_error: false,
        }),
        progress: String::new(),
        timing: crate::transcript::ToolTiming::default(),
    };
    let rows = tool_output(&command, ToolKind::Execute, false, 60, Mode::Truncated);
    let joined: Vec<String> = rows.iter().map(|row| text(&row.line)).collect();
    assert_eq!(joined[0], "");
    assert!(!rows[0].panel, "the separator is not a band");
    assert_eq!(
        joined.len(),
        1 + EXECUTE_FIRST + 1 + EXECUTE_LAST,
        "{joined:?}"
    );
    assert_eq!(joined[1 + EXECUTE_FIRST], "\u{2026} +7 lines");
    assert_eq!(
        rows[1 + EXECUTE_FIRST].line.spans[0].style.fg,
        theme::style(Token::Muted).fg,
        "execute.rs:553 paints the truncation line muted, not dim"
    );
    assert!(
        rows[1..].iter().all(|row| row.panel),
        "output sits on a band"
    );

    let file = ToolItem {
        name: "read_file".into(),
        args: serde_json::json!({ "path": "a.rs" }),
        call_id: String::new(),
        output: Some(crate::transcript::ToolItemOutput {
            content: (1..=12)
                .map(|n| format!("line {n}"))
                .collect::<Vec<_>>()
                .join("\n"),
            is_error: false,
        }),
        progress: String::new(),
        timing: crate::transcript::ToolTiming::default(),
    };
    let rows = tool_output(&file, ToolKind::Read, false, 60, Mode::Truncated);
    let joined: Vec<String> = rows.iter().map(|row| text(&row.line)).collect();
    assert_eq!(joined[0], "");
    assert!(!rows[0].panel, "the separator is not a band");
    // A bare `…` with no count: the line-number gutter says how much is gone.
    assert_eq!(joined[1 + READ_FIRST], "\u{2026}", "{joined:?}");
    assert_eq!(
        rows[1 + READ_FIRST].line.spans[0].style.fg,
        theme::style(Token::Muted).fg,
        "read.rs:305 paints the ellipsis muted, not dim like the gutter"
    );
    // The gutter is right-aligned to the width of the largest number.
    assert!(joined[1].starts_with(" 1  line 1"), "{joined:?}");
    assert!(
        joined[1 + READ_FIRST + 1].starts_with("10  line 10"),
        "{joined:?}"
    );
    assert!(
        rows[1..].iter().all(|row| row.panel),
        "a file read sits on bg_dark"
    );

    let ranged = ToolItem {
        name: "read_file".into(),
        args: serde_json::json!({ "path": "a.rs", "start_line": 50 }),
        call_id: String::new(),
        output: Some(crate::transcript::ToolItemOutput {
            content: "fn foo() {}\nfn bar() {}\nfn baz() {}".into(),
            is_error: false,
        }),
        progress: String::new(),
        timing: crate::transcript::ToolTiming::default(),
    };
    let rows = tool_output(&ranged, ToolKind::Read, false, 60, Mode::Truncated);
    let joined: Vec<String> = rows.iter().map(|row| text(&row.line)).collect();
    assert_eq!(joined[0], "");
    assert!(joined[1].starts_with("50  "), "{joined:?}");
    assert!(joined[3].starts_with("52  "), "{joined:?}");
}

#[test]
fn a_running_command_uses_the_first_last_window() {
    let streaming = ToolItem {
        name: "execute".into(),
        args: serde_json::json!({ "command": "cargo test" }),
        call_id: String::new(),
        output: None,
        progress: (1..=12)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n"),
        timing: crate::transcript::ToolTiming::default(),
    };
    let rows = tool_output(&streaming, ToolKind::Execute, true, 60, Mode::Truncated);
    let joined: Vec<String> = rows.iter().map(|row| text(&row.line)).collect();
    assert_eq!(
        joined.len(),
        1 + EXECUTE_FIRST + 1 + EXECUTE_LAST,
        "{joined:?}"
    );
    assert_eq!(joined[1], "line 1");
    assert_eq!(joined[2], "line 2");
    assert_eq!(joined[1 + EXECUTE_FIRST], "\u{2026} +7 lines");
    assert_eq!(
        &joined[joined.len() - EXECUTE_LAST..],
        ["line 10", "line 11", "line 12"]
    );
}

#[test]
fn execute_and_read_window_on_wrapped_count_not_raw_lines() {
    // One raw line that wraps past first+last must still window: grok-build
    // wraps the body, then takes the first/last *wrapped* rows.
    let long = "x".repeat(400);
    let command = sample_tool("execute", serde_json::json!({ "command": "echo" }), &long);
    let rows = tool_output(&command, ToolKind::Execute, false, 60, Mode::Truncated);
    let joined: Vec<String> = rows.iter().map(|row| text(&row.line)).collect();
    assert_eq!(joined[0], "");
    assert!(
        joined.iter().any(|s| s.contains('\u{2026}')),
        "wrap-then-truncate must window a long command, got {joined:?}"
    );

    let file = sample_tool("read_file", serde_json::json!({ "path": "a.rs" }), &long);
    let rows = tool_output(&file, ToolKind::Read, false, 60, Mode::Truncated);
    let joined: Vec<String> = rows.iter().map(|row| text(&row.line)).collect();
    assert_eq!(joined[0], "");
    assert!(
        joined.iter().any(|s| s == "\u{2026}"),
        "wrap-then-truncate must window a long file, got {joined:?}"
    );
}

#[test]
fn execute_and_read_bodies_are_primary_on_the_panel() {
    let _theme = grok_theme();
    let primary = theme::style(Token::Text).fg;
    let muted = theme::style(Token::Muted).fg;
    assert_ne!(primary, muted, "the test needs Text and Muted to differ");

    let command = sample_tool(
        "execute",
        serde_json::json!({ "command": "echo" }),
        "hello from the shell",
    );
    let rows = tool_output(&command, ToolKind::Execute, false, 60, Mode::Truncated);
    let content = &rows[1].line;
    assert!(
        content.spans.iter().any(|s| s.style.fg == primary),
        "execute body should be primary, got {:?}",
        content.spans
    );
    assert!(
        content
            .spans
            .iter()
            .filter(|s| !s.content.trim().is_empty())
            .all(|s| s.style.fg != muted),
        "execute body should not sit in muted, got {:?}",
        content.spans
    );

    let file = sample_tool(
        "read_file",
        serde_json::json!({ "path": "a.rs" }),
        "fn foo() {}",
    );
    let rows = tool_output(&file, ToolKind::Read, false, 60, Mode::Truncated);
    let line = &rows[1].line;
    assert_eq!(
        line.spans[0].style.fg,
        theme::style(Token::Faint).fg,
        "gutter stays dim"
    );
    assert!(
        line.spans
            .iter()
            .skip(1)
            .filter(|s| !s.content.is_empty())
            .all(|s| s.style.fg != muted),
        "read body should not sit in muted, got {:?}",
        line.spans
    );
}

#[test]
fn read_body_maps_syntect_scopes_onto_tokens() {
    let _theme = grok_theme();
    let file = sample_tool(
        "read_file",
        serde_json::json!({ "path": "a.rs" }),
        "fn foo() { let x = \"hi\"; }",
    );
    let rows = tool_output(&file, ToolKind::Read, false, 80, Mode::Truncated);
    let line = &rows[1].line;
    let body: Vec<_> = line
        .spans
        .iter()
        .skip(1)
        .filter(|s| !s.content.is_empty())
        .collect();
    let fg_of = |token: Token| theme::style(token).fg;
    assert!(
        body.iter().any(|s| s.style.fg == fg_of(Token::Accent)),
        "keyword scope maps to Accent, got {body:?}"
    );
    assert!(
        body.iter().any(|s| s.style.fg == fg_of(Token::Success)),
        "string scope maps to Success, got {body:?}"
    );
}

#[test]
fn read_body_wraps_at_content_width_minus_gutter() {
    let _theme = grok_theme();
    let file = sample_tool(
        "read_file",
        serde_json::json!({ "path": "a.rs" }),
        &"a".repeat(53),
    );
    let rows = tool_output(&file, ToolKind::Read, false, 60, Mode::Truncated);
    let panel: Vec<_> = rows.iter().filter(|row| row.panel).collect();
    assert!(
        panel.len() >= 2,
        "53-col source at width 60 wraps after content_width minus gutter, got {}",
        panel.len()
    );
}

#[test]
fn execute_keeps_sgr_text_and_maps_red_through_error() {
    let command = sample_tool(
        "execute",
        serde_json::json!({ "command": "cargo test" }),
        "\u{1b}[31mFAIL\u{1b}[0m ok",
    );
    let rows = tool_output(&command, ToolKind::Execute, false, 60, Mode::Truncated);
    let joined = text(&rows[1].line);
    assert_eq!(joined, "FAIL ok");
    assert!(
        !joined.contains('\u{1b}'),
        "CSI must not leak onto the card"
    );
    let fail = rows[1]
        .line
        .spans
        .iter()
        .find(|s| s.content.contains("FAIL"))
        .expect("FAIL span");
    assert_eq!(fail.style.fg, theme::style(Token::Error).fg);
}

fn sample_tool(name: &str, args: serde_json::Value, content: &str) -> ToolItem {
    ToolItem {
        name: name.into(),
        args,
        call_id: String::new(),
        output: Some(crate::transcript::ToolItemOutput {
            content: content.into(),
            is_error: false,
        }),
        progress: String::new(),
        timing: crate::transcript::ToolTiming::default(),
    }
}

#[test]
fn list_dir_sits_on_a_panel_after_a_blank_separator() {
    let tool = sample_tool(
        "list_files",
        serde_json::json!({ "path": "src" }),
        "app/\nui/\ngrok.rs",
    );
    let entry = tool_entry(&tool, &app(), false, 0, 60);
    assert_eq!(entry.mode, Mode::Truncated);
    let rows = tool_output(&tool, ToolKind::ListDir, false, 60, Mode::Truncated);
    let joined: Vec<String> = rows.iter().map(|row| text(&row.line)).collect();
    assert_eq!(joined, ["", "  app/", "  ui/", "  grok.rs"]);
    assert!(!rows[0].panel, "the separator is not a band");
    assert!(rows[1].panel && rows[2].panel && rows[3].panel);
}

#[test]
fn search_groups_hits_under_the_file_and_a_metadata_line() {
    let tool = sample_tool(
        "search_files",
        serde_json::json!({ "pattern": "todo", "path": "src" }),
        "src/a.rs:1:todo\nsrc/a.rs:9:todo again\nsrc/b.rs:2:todo",
    );
    let rows = tool_output(&tool, ToolKind::Search, false, 60, Mode::Truncated);
    let joined: Vec<String> = rows.iter().map(|row| text(&row.line)).collect();
    assert_eq!(
        joined,
        [
            "",
            "  mode: pattern",
            "",
            "  src/a.rs",
            "       1  todo",
            "       9  todo again",
            "",
            "  src/b.rs",
            "       2  todo",
        ]
    );
    assert!(!rows[0].panel && !rows[1].panel && !rows[2].panel);
    assert!(rows[3].panel && rows[4].panel && rows[5].panel);
    assert!(!rows[6].panel);
    assert!(rows[7].panel && rows[8].panel);
    let meta = &rows[1].line.spans;
    assert_eq!(meta[1].content.as_ref(), "mode: ");
    assert_eq!(meta[1].style.fg, theme::style(Token::Muted).fg);
    assert_eq!(meta[2].content.as_ref(), "pattern");
    assert_eq!(meta[2].style.fg, theme::style(Token::Text).fg);
    assert_eq!(rows[3].line.spans[1].style.fg, theme::style(Token::Link).fg);
    assert_eq!(rows[4].line.spans[1].content.as_ref(), "   1");
    assert_eq!(
        rows[4].line.spans[1].style.fg,
        theme::style(Token::Muted).fg
    );
}

#[test]
fn search_body_paints_muted_no_results_after_metadata() {
    let _theme = grok_theme();
    let tool = sample_tool("search_files", serde_json::json!({ "pattern": "todo" }), "");
    let rows = tool_output(&tool, ToolKind::Search, false, 80, Mode::Truncated);
    let joined: Vec<String> = rows.iter().map(|row| text(&row.line)).collect();
    assert_eq!(joined, ["", "  mode: pattern", "", "  (no results)"]);
    assert_eq!(
        rows[3].line.spans.last().unwrap().style.fg,
        theme::style(Token::Muted).fg,
        "search.rs:406 paints (no results) muted"
    );
}

#[test]
fn search_metadata_joins_optional_fields_with_commas() {
    let tool = sample_tool(
        "search_files",
        serde_json::json!({
            "pattern": "todo",
            "file_type": "rs",
            "case_insensitive": true,
            "multiline": true
        }),
        "src/a.rs:1:todo",
    );
    let rows = tool_output(&tool, ToolKind::Search, false, 80, Mode::Truncated);
    let meta = text(&rows[1].line);
    assert_eq!(
        meta,
        "  mode: pattern, type: rs, case-insensitive: true, multiline: true"
    );
    assert!(
        !rows[1]
            .line
            .spans
            .iter()
            .any(|span| span.content.as_ref() == "pattern: "),
        "pattern is the mode value, not a field: {:?}",
        rows[1]
            .line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<Vec<_>>()
    );
}

#[test]
fn search_files_with_matches_are_indented_path_panel_rows() {
    let tool = sample_tool(
        "search_files",
        serde_json::json!({ "pattern": "todo", "output_mode": "files_with_matches" }),
        "src/a.rs\nsrc/b.rs",
    );
    let rows = tool_output(&tool, ToolKind::Search, false, 60, Mode::Truncated);
    let joined: Vec<String> = rows.iter().map(|row| text(&row.line)).collect();
    assert_eq!(
        joined,
        ["", "  mode: files", "", "  src/a.rs", "  src/b.rs"]
    );
    assert!(!rows[0].panel && !rows[1].panel && !rows[2].panel);
    assert!(rows[3].panel && rows[4].panel);
    assert_eq!(rows[3].line.spans[1].style.fg, theme::style(Token::Link).fg);
    assert_eq!(rows[4].line.spans[1].style.fg, theme::style(Token::Link).fg);
}

#[test]
fn search_count_splits_the_colon_n_on_path_panel_rows() {
    let tool = sample_tool(
        "search_files",
        serde_json::json!({ "pattern": "todo", "output_mode": "count" }),
        "src/a.rs:3\nsrc/b.rs:1",
    );
    let rows = tool_output(&tool, ToolKind::Search, false, 60, Mode::Truncated);
    let joined: Vec<String> = rows.iter().map(|row| text(&row.line)).collect();
    assert_eq!(
        joined,
        ["", "  mode: count", "", "  src/a.rs:3", "  src/b.rs:1"]
    );
    assert!(!rows[0].panel && !rows[1].panel && !rows[2].panel);
    assert!(rows[3].panel && rows[4].panel);
    let spans = &rows[3].line.spans;
    assert_eq!(spans[1].content.as_ref(), "src/a.rs");
    assert_eq!(spans[1].style.fg, theme::style(Token::Link).fg);
    assert_eq!(spans[2].content.as_ref(), ":3");
    assert_eq!(spans[2].style.fg, theme::style(Token::Text).fg);
}

#[test]
fn edit_reconstructs_the_change_after_a_blank_separator() {
    let tool = sample_tool(
        "edit_file",
        serde_json::json!({
            "path": "src/x.rs",
            "old_string": "let a = 1;",
            "new_string": "let a = 2;\nlet b = 3;",
        }),
        "Edited src/x.rs: replaced 1 occurrence",
    );
    let rows = tool_output(&tool, ToolKind::Edit, false, 60, Mode::Truncated);
    let joined: Vec<String> = rows.iter().map(|row| text(&row.line)).collect();
    assert_eq!(
        joined,
        ["", "  -let a = 1;", "  +let a = 2;", "  +let b = 3;"]
    );
    assert!(rows.iter().all(|row| !row.panel));
}

#[test]
fn other_shows_every_line_and_mcp_stays_capped() {
    let many: String = (1..=15)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    let other = sample_tool("memory", serde_json::json!({}), &many);
    let rows = tool_output(&other, ToolKind::Other, false, 60, Mode::Truncated);
    let joined: Vec<String> = rows.iter().map(|row| text(&row.line)).collect();
    assert_eq!(joined.len(), 16, "{joined:?}");
    assert_eq!(joined[0], "");
    assert_eq!(joined[15], "line 15");
    assert!(joined.iter().all(|line| !line.contains("more lines")));
    assert!(rows.iter().all(|row| !row.panel));

    let mcp = sample_tool("brave__browser_click", serde_json::json!({}), &many);
    let rows = tool_output(&mcp, ToolKind::Other, false, 60, Mode::Truncated);
    let joined: Vec<String> = rows.iter().map(|row| text(&row.line)).collect();
    assert_eq!(joined[0], "");
    assert_eq!(joined[1], "");
    assert_eq!(joined[2], "  line 1");
    assert_eq!(joined[4], "  line 3");
    assert!(joined[5].contains("12 more lines"), "{joined:?}");
    assert!(rows[1].panel && rows[2].panel);
}

#[test]
fn mcp_shows_key_val_input_rows_before_the_output_panel() {
    let mcp = sample_tool(
        "linear__save_issue",
        serde_json::json!({ "title": "Bug", "id": 7 }),
        "ok\ndone",
    );
    let rows = tool_output(&mcp, ToolKind::Other, false, 60, Mode::Truncated);
    let joined: Vec<String> = rows.iter().map(|row| text(&row.line)).collect();
    assert_eq!(
        joined,
        ["", "  title: Bug", "  id: 7", "", "", "  ok", "  done"]
    );
    assert!(!rows[0].panel && !rows[1].panel && !rows[2].panel);
    assert!(!rows[3].panel && rows[4].panel && rows[5].panel);
    assert_eq!(
        rows[1].line.spans[0].style.fg,
        theme::style(Token::Muted).fg
    );
    assert_eq!(rows[1].line.spans[1].style.fg, theme::style(Token::Text).fg);

    let pending = ToolItem {
        name: "linear__save_issue".into(),
        args: serde_json::json!({ "title": "Bug" }),
        call_id: String::new(),
        output: None,
        progress: String::new(),
        timing: crate::transcript::ToolTiming::default(),
    };
    let rows = tool_output(&pending, ToolKind::Other, false, 60, Mode::Truncated);
    let joined: Vec<String> = rows.iter().map(|row| text(&row.line)).collect();
    assert_eq!(joined, ["", "  title: Bug"]);
    assert!(rows.iter().all(|row| !row.panel));
}

#[test]
fn fetch_and_web_search_sit_in_a_primary_content_box() {
    let _theme = grok_theme();
    let primary = theme::style(Token::Text).fg;
    let muted = theme::style(Token::Muted).fg;
    assert_ne!(primary, muted, "the test needs Text and Muted to differ");

    let empty = sample_tool(
        "web_fetch",
        serde_json::json!({ "url": "https://x.com" }),
        "",
    );
    let rows = tool_output(&empty, ToolKind::Fetch, false, 60, Mode::Truncated);
    let joined: Vec<String> = rows.iter().map(|row| text(&row.line)).collect();
    assert_eq!(joined, ["", "  (no content)"]);
    assert!(rows.iter().all(|row| !row.panel));
    assert_eq!(rows[1].line.spans.last().unwrap().style.fg, muted);

    let many: String = (1..=15)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    let fetch = sample_tool(
        "web_fetch",
        serde_json::json!({ "url": "https://x.com" }),
        &many,
    );
    let rows = tool_output(&fetch, ToolKind::Fetch, false, 60, Mode::Truncated);
    let joined: Vec<String> = rows.iter().map(|row| text(&row.line)).collect();
    assert_eq!(joined[0], "");
    assert_eq!(joined[1], "");
    assert_eq!(joined[2], "  line 1");
    assert_eq!(joined[4], "  line 3");
    assert!(joined[5].contains("12 more lines"), "{joined:?}");
    assert_eq!(joined[6], "");
    assert!(!rows[0].panel);
    assert!(rows[1].panel && rows[2].panel && rows[5].panel && rows[6].panel);
    assert_eq!(rows[2].line.spans.last().unwrap().style.fg, primary);

    let search = sample_tool(
        "web_search",
        serde_json::json!({ "query": "grok" }),
        "hello\nworld",
    );
    let rows = tool_output(&search, ToolKind::WebSearch, false, 60, Mode::Expanded);
    let joined: Vec<String> = rows.iter().map(|row| text(&row.line)).collect();
    assert_eq!(joined, ["", "", "  hello", "  world", ""]);
    assert!(!rows[0].panel && rows[1].panel && rows[4].panel);
    assert_eq!(rows[2].line.spans.last().unwrap().style.fg, primary);
}

// ── the screen ──────────────────────────────────────────────────────────

#[test]
fn the_frame_keeps_grok_builds_margins_and_puts_the_scrollbar_last() {
    let mut app = app();
    app.transcript.user("hello".to_string(), Vec::new());
    let layout = screen(&app, Rect::new(0, 0, 80, 24), 0, 3);
    // Two columns of outer margin either side: the block runs 2..=77, and
    // the scrollbar shares the right margin at column 79.
    assert_eq!(layout.body.x, 2);
    assert_eq!(layout.body.right(), 78);
    assert_eq!(layout.body.width, 76);
    // Content inside that: rail, two pads, 71 columns of prose, two pads.
    assert_eq!(content_width(layout.body.width), 71);
    assert_eq!(layout.composer.x, 2);
    assert_eq!(layout.composer.height, 3);
    // A shortcuts bar at the bottom, above one row of outer margin.
    assert_eq!(layout.shortcuts.bottom(), 23);
}

#[test]
fn a_turn_in_flight_gets_a_status_row_and_an_idle_one_does_not() {
    let mut app = app();
    assert!(turn_status(&app).is_none(), "idle says nothing");
    app.status.busy = true;
    let line = turn_status(&app)
        .expect("a running turn narrates itself")
        .line(76);
    let rendered = text(&line);
    assert!(rendered.contains("[stop]"), "{rendered}");
    assert!(rendered.contains("step 0"), "{rendered}");
    // Upstream's fixed wording, not a shuffled verb pool — and Wizard's
    // own step counter, which no skin gets to withhold.
    assert!(rendered.contains("Thinking\u{2026}"), "{rendered}");
}

#[test]
fn the_status_row_freezes_into_a_diamond_when_the_agent_is_blocked_on_you() {
    let mut app = app();
    app.status.busy = true;
    let (gate, _rx) = crate::agent::PlanGate::open();
    app.handle_agent_event(AgentEvent::PlanReady {
        plan: "do the thing".to_string(),
        gate,
    });
    let line = turn_status(&app)
        .expect("a blocked turn still narrates")
        .line(76);
    let rendered = text(&line);
    assert!(rendered.starts_with(DIAMOND), "{rendered}");
    assert!(rendered.contains("plan review"), "{rendered}");
    assert!(
        !rendered.contains("[stop]"),
        "no cancel while parked: {rendered}"
    );
}

#[test]
fn the_turn_arrow_counts_context_not_the_session_lifetime() {
    let mut app = app();
    app.status.busy = true;
    // Each model call re-sends the history, so the lifetime sum (what `/cost`
    // prices) runs far ahead of the window. The ⇣ must not follow it.
    app.status.prompt_tokens = 400_000;
    app.status.completion_tokens = 80_000;
    app.status.context_tokens = 12_300;
    let rendered = text(&turn_status(&app).expect("busy").line(80));
    assert!(
        rendered.contains("\u{21e3}12.3k"),
        "arrow should be context used, got {rendered}"
    );
    assert!(
        !rendered.contains("480"),
        "session lifetime must not land on the arrow: {rendered}"
    );
}

#[test]
fn an_idle_session_with_background_work_still_says_so() {
    let mut app = app();
    app.status.background_tasks = 1;
    app.status.background_subagents = 2;
    let rendered = text(
        &turn_status(&app)
            .expect("background work is narrated")
            .line(76),
    );
    assert!(
        rendered.contains("1 command \u{00b7} 2 subagents still running"),
        "{rendered}"
    );
}

#[test]
fn wizards_own_state_survives_wearing_someone_elses_chrome() {
    let mut app = app();
    app.status.busy = true;
    app.status.background_subagents = 1;
    app.status.context_tokens = 8_500;
    app.plan_mode = true;
    app.handle_agent_event(AgentEvent::SubagentRunStarted {
        run: 1,
        bg: Some(1),
        name: "researcher".to_string(),
        task: "map the auth flow".to_string(),
    });
    let screen = render(&app, 100, 30).join("\n");
    assert!(screen.contains("plan"), "the mode is on the composer");
    assert!(screen.contains("genie"), "and so is the mode word");
    assert!(screen.contains("sub"), "background subagents are chipped");
    // The subagent survives, in the tasks pane's own grammar: a `▾
    // Subagents 1` header over a capitalized persona row.
    assert!(screen.contains("Subagents 1"), "the tasks pane groups them");
    assert!(screen.contains("Researcher"), "and names the run");
    assert!(
        screen.contains("8.5K / 150K"),
        "context chip is used / total: {screen}"
    );
    assert!(
        screen.contains(&format!("{DIAMOND} 1 sub")),
        "task chip is a static diamond: {screen}"
    );
    assert!(
        !screen.contains("Ctrl+t"),
        "idle expand is not a grok-build shortcut: {screen}"
    );
}

#[test]
fn fmt_tokens_matches_grok_builds_uppercase_form() {
    assert_eq!(fmt_tokens(0), "0");
    assert_eq!(fmt_tokens(999), "999");
    assert_eq!(fmt_tokens(1_000), "1.0K");
    assert_eq!(fmt_tokens(8_500), "8.5K");
    assert_eq!(fmt_tokens(10_000), "10K");
    assert_eq!(fmt_tokens(150_000), "150K");
    assert_eq!(fmt_tokens(1_200_000), "1.2M");
    assert_eq!(fmt_tokens(12_000_000), "12M");
}

#[test]
fn compact_shortcuts_keep_the_first_five_and_pin_help() {
    let pairs = [
        ("Enter", "send"),
        ("Shift+Enter", "newline"),
        ("/", "commands"),
        ("Shift+Tab", "mode"),
        ("Ctrl+c", "quit"),
        ("PgUp/PgDn", "scroll"),
    ];
    assert_eq!(
        compact_shortcut_pairs(&pairs, Some(("?", "help")), 5),
        vec![
            ("Enter", "send"),
            ("Shift+Enter", "newline"),
            ("/", "commands"),
            ("Shift+Tab", "mode"),
            ("Ctrl+c", "quit"),
            ("?", "help"),
        ]
    );
}

#[test]
fn idle_shortcuts_pin_help_and_busy_ones_keep_mode() {
    let idle = app();
    let idle_pairs = compact_shortcut_pairs(&shortcut_pairs(&idle), Some(("?", "help")), 5);
    assert!(idle_pairs.contains(&("?", "help")), "{idle_pairs:?}");
    assert!(
        idle_pairs.contains(&("Shift+Tab", "mode")),
        "{idle_pairs:?}"
    );
    assert!(
        !idle_pairs.iter().any(|&(k, _)| k == "Ctrl+t"),
        "{idle_pairs:?}"
    );

    let mut busy = app();
    busy.status.busy = true;
    let busy_pairs = compact_shortcut_pairs(&shortcut_pairs(&busy), Some(("?", "help")), 5);
    assert!(busy_pairs.contains(&("Enter", "queue")), "{busy_pairs:?}");
    assert!(
        busy_pairs.contains(&("Shift+Tab", "mode")),
        "{busy_pairs:?}"
    );
    assert!(busy_pairs.contains(&("?", "help")), "{busy_pairs:?}");
}

#[test]
fn the_status_bar_chips_plan_and_mcp_in_grok_builds_order() {
    let mut app = app();
    app.plan_mode = true;
    app.mcp_connecting = true;
    app.status.background_tasks = 2;
    app.status.context_tokens = 8_500;
    let spans = status_chip_spans(&app);
    let rendered: String = spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(rendered.contains(&format!("{DIAMOND} 2")), "{rendered}");
    assert!(rendered.contains("plan"), "{rendered}");
    assert!(rendered.contains("MCP"), "{rendered}");
    assert!(rendered.contains("8.5K / 150K"), "{rendered}");
    assert!(!rendered.contains("tools"), "{rendered}");
    let diamond_at = rendered.find(&format!("{DIAMOND} 2")).expect("tasks");
    let plan_at = rendered.find("plan").expect("plan");
    let mcp_at = rendered.find("MCP").expect("mcp");
    let ctx_at = rendered.find("8.5K / 150K").expect("context");
    assert!(
        diamond_at < plan_at && plan_at < mcp_at && mcp_at < ctx_at,
        "{rendered}"
    );
}

#[test]
fn a_prompt_and_a_reply_render_in_their_own_columns() {
    let mut app = app();
    app.transcript
        .user("add a --json flag".to_string(), Vec::new());
    app.transcript.assistant("Sure, I'll do that.".to_string());
    app.transcript.commit();
    let rows = render(&app, 80, 24);
    let prompt = rows
        .iter()
        .find(|row| row.contains("add a --json flag"))
        .expect("the prompt is on screen");
    // Two columns of outer margin, one of (cleared) rail, two of pad, then
    // the `❯ `.
    assert!(
        prompt.starts_with("     \u{276f} add a --json flag"),
        "{prompt:?}"
    );
    let reply = rows
        .iter()
        .find(|row| row.contains("Sure"))
        .expect("the reply is on screen");
    // Same column, no marker: an agent message has no rail and no bullet.
    assert!(reply.starts_with("     Sure"), "{reply:?}");
}

#[test]
fn the_welcome_screen_swaps_shape_at_ninety_columns() {
    let mut app = App::new(Config::default());
    app.status.model = "wizard-1".to_string();
    // The hero box is the only bordered thing on the whole screen.
    let wide = render(&app, 100, 30).join("\n");
    assert!(wide.contains('\u{256d}'), "a rounded box at 100 columns");
    assert!(wide.contains("Wizard"), "{wide}");
    // Below 90 it stacks, and the box is gone (bar the composer's own).
    let narrow = render(&app, 70, 30);
    let boxed = narrow.iter().filter(|row| row.contains('\u{256d}')).count();
    assert_eq!(boxed, 1, "only the composer is boxed below 90 columns");

    // Under both, the menu is a column: labels flush left, keys flush right
    // at a common edge. Centring the rows one at a time — which is what
    // this did before the rows were padded to `MENU_WIDTH` — puts every
    // key in a different place and the menu stops reading as one.
    for rows in [render(&app, 100, 30), narrow] {
        // Flush *right*, so what lines up is where each key ends. Counted
        // in cells, not bytes: the hero box carries the three-byte braille
        // mark to the left of the menu.
        let key_end = |needle: &str| {
            rows.iter().find(|row| row.contains(needle)).map(|row| {
                let at = row.find(needle).expect("just matched");
                row[..at].chars().count() + needle.chars().count()
            })
        };
        assert_eq!(
            key_end("/model"),
            key_end("/help"),
            "the keys share a right edge:\n{}",
            rows.join("\n")
        );
    }
}

#[test]
fn a_tiny_terminal_still_gets_a_composer() {
    // Everything degrades before the composer does: the margins and the
    // gap rows are the first thing spent.
    let rows = render(&app(), 40, 8);
    assert!(
        rows.iter().any(|row| row.contains('\u{276f}')),
        "the prompt survives:\n{}",
        rows.join("\n")
    );
}

fn slash_app(items: &[(&str, &str)]) -> App {
    let mut app = app();
    app.suggestions = items
        .iter()
        .map(|(name, desc)| crate::app::Suggestion {
            name: (*name).to_string(),
            args: String::new(),
            description: (*desc).to_string(),
            takes_args: false,
        })
        .collect();
    app.suggestion_index = 0;
    app
}

#[test]
fn the_slash_popup_is_not_a_rounded_box() {
    let app = slash_app(&[
        ("init", "set up the project"),
        ("help", "show commands"),
        ("quit", "leave"),
    ]);
    let dump = render(&app, 80, 24).join("\n");
    let corners = dump.matches('\u{256d}').count();
    assert_eq!(corners, 1, "only the composer is a rounded box:\n{dump}");
    assert!(
        dump.contains('\u{2500}'),
        "the dropdown has hairlines:\n{dump}"
    );
}

#[test]
fn the_slash_popup_puts_the_item_count_on_the_top_rule() {
    let app = slash_app(&[
        ("init", "set up the project"),
        ("help", "show commands"),
        ("quit", "leave"),
    ]);
    let rows = render(&app, 80, 24);
    let dump = rows.join("\n");
    let item = rows
        .iter()
        .position(|row| row.contains("/init"))
        .expect("/init is on screen");
    assert!(item > 0, "there is a row above /init:\n{dump}");
    let rule = &rows[item - 1];
    assert!(
        rule.contains('\u{2500}'),
        "the row above /init is a hairline:\n{dump}"
    );
    assert!(
        rule.contains('3'),
        "the match count sits on the top rule:\n{dump}"
    );
    let bottom = rows
        .iter()
        .skip(item + 1)
        .find(|row| row.contains('\u{2500}') && !row.contains('\u{256d}'))
        .expect("bottom hairline");
    assert!(
        !bottom.contains('3'),
        "the count is not on the bottom rule:\n{dump}"
    );
}

#[test]
fn the_slash_popup_marks_the_selected_row_with_the_prompt_arrow() {
    let mut app = slash_app(&[("init", "set up"), ("help", "show commands")]);
    let rows = render(&app, 80, 24);
    let dump = rows.join("\n");
    let init = rows
        .iter()
        .find(|row| row.contains("/init"))
        .expect("/init");
    let help = rows
        .iter()
        .find(|row| row.contains("/help"))
        .expect("/help");
    assert!(init.contains('\u{276f}'), "selected row carries ❯:\n{dump}");
    assert!(
        !help.contains('\u{276f}'),
        "unselected row does not:\n{dump}"
    );

    app.suggestion_index = 1;
    let rows = render(&app, 80, 24);
    let dump = rows.join("\n");
    let init = rows
        .iter()
        .find(|row| row.contains("/init"))
        .expect("/init");
    let help = rows
        .iter()
        .find(|row| row.contains("/help"))
        .expect("/help");
    assert!(
        !init.contains('\u{276f}'),
        "index 1 leaves /init bare:\n{dump}"
    );
    assert!(help.contains('\u{276f}'), "index 1 marks /help:\n{dump}");
}

#[test]
fn the_slash_popup_wraps_a_long_description_onto_the_next_row() {
    let app = slash_app(&[(
        "init",
        "one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty twentyone twentytwo twentythree twentyfour twentyfive",
    )]);
    let rows = render(&app, 80, 24);
    let dump = rows.join("\n");
    let start = rows
        .iter()
        .position(|row| row.contains("/init"))
        .expect("/init");
    assert!(
        rows[start].contains("/init"),
        "the name is on the first row:\n{dump}"
    );
    assert!(
        start + 1 < rows.len(),
        "there is a row under /init:\n{dump}"
    );
    let cont = &rows[start + 1];
    assert!(
        !cont.contains('\u{276f}'),
        "the wrap row has no second arrow:\n{dump}"
    );
    assert!(
        cont.contains("twenty") || cont.contains("fifteen") || cont.contains("sixteen"),
        "a later word landed on the wrap row:\n{dump}"
    );
}

#[test]
fn the_slash_popup_grows_taller_than_the_item_count_when_a_description_wraps() {
    let app = slash_app(&[(
        "init",
        "one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty twentyone twentytwo twentythree twentyfour twentyfive",
    )]);
    let rows = render(&app, 80, 24);
    let dump = rows.join("\n");
    let start = rows
        .iter()
        .position(|row| row.contains("/init"))
        .expect("/init");
    let mut inner = 1usize;
    for row in rows.iter().skip(start + 1) {
        if row.contains('\u{2500}') || row.contains('\u{256d}') {
            break;
        }
        inner += 1;
    }
    assert!(
        inner > 1,
        "wrapped description made the popup taller than one item:\n{dump}"
    );
}

#[test]
fn slash_preview_ghosts_the_rest_of_the_name() {
    let mut app = slash_app(&[("help", "show help")]);
    app.input = "/he".into();
    let preview = super::slash_preview(&app).expect("slash");
    assert_eq!(preview.cmd_end, 3);
    assert_eq!(preview.remainder, "lp");
    assert!(preview.args.is_empty());
}

#[test]
fn slash_preview_ghosts_the_args_placeholder() {
    let mut app = slash_app(&[("model", "pick a model")]);
    app.suggestions[0].takes_args = true;
    app.suggestions[0].args = "<name>".into();
    app.input = "/model".into();
    let preview = super::slash_preview(&app).expect("slash");
    assert_eq!(preview.remainder, "");
    assert_eq!(preview.args, " <name>");
}

#[test]
fn slash_preview_drops_the_args_ghost_once_the_user_types_a_space() {
    let mut app = slash_app(&[("model", "pick a model")]);
    app.suggestions[0].takes_args = true;
    app.suggestions[0].args = "<name>".into();
    app.input = "/model ".into();
    let preview = super::slash_preview(&app).expect("slash");
    assert!(preview.args.is_empty());
}

#[test]
fn the_composer_ghosts_the_rest_of_a_partial_slash_command() {
    let mut app = slash_app(&[("help", "show help")]);
    app.input = "/he".into();
    app.cursor = 3;
    let rows = render(&app, 80, 24);
    let dump = rows.join("\n");
    let composer = rows
        .iter()
        .rfind(|row| row.contains('\u{276f}') && row.contains("/help"))
        .unwrap_or_else(|| panic!("composer should show /he plus ghost lp:\n{dump}"));
    assert!(
        composer.contains("/help"),
        "remainder lp sits after /he:\n{dump}"
    );
}

#[test]
fn the_composer_recolors_the_slash_command_token() {
    let _theme = grok_theme();
    let mut app = slash_app(&[("help", "show help")]);
    app.input = "/he".into();
    app.cursor = 3;
    let buffer = render_buffer(&app, 80, 24);
    let mut found = None;
    for y in (0..24u16).rev() {
        for x in 0..80u16 {
            if buffer[(x, y)].symbol() == "/"
                && x + 1 < 80
                && buffer[(x + 1, y)].symbol() == "h"
                && x + 2 < 80
                && buffer[(x + 2, y)].symbol() == "e"
            {
                found = Some((x, y));
                break;
            }
        }
        if found.is_some() {
            break;
        }
    }
    let (x, y) = found.expect("the /he token is on screen");
    let heading = crate::theme::color(Token::Heading);
    let muted = crate::theme::color(Token::Muted);
    assert_eq!(
        buffer[(x, y)].fg,
        heading,
        "/ should be the command token colour"
    );
    assert_eq!(buffer[(x + 1, y)].fg, heading, "h should match /");
    assert_eq!(buffer[(x + 2, y)].fg, heading, "e should match /");
    assert_eq!(
        buffer[(x + 3, y)].fg,
        muted,
        "ghost l should be muted, not italic"
    );
    assert!(
        !buffer[(x + 3, y)]
            .modifier
            .contains(ratatui::style::Modifier::ITALIC)
    );
}

#[test]
fn the_animation_speeds_are_upstreams_rescaled_to_wizards_clock() {
    // Upstream animates at 30fps; Wizard ticks at 10Hz. The wall-clock
    // cadence is what a person sees, so the speeds are tripled, not copied.
    assert!((WAVE_SPEED - 0.45).abs() < 1e-6);
    assert!((PULSE_SPEED - 0.24).abs() < 1e-6);
    // And a wave still stays inside the unit interval at every row.
    for tick in 0..40u64 {
        for row in 0..8u16 {
            let value = motion::wave(tick, row, WAVE_ROWS, WAVE_SPEED);
            assert!((0.0..=1.0).contains(&value));
        }
    }
}
