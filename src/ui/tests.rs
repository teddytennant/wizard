use super::*;

/// Flatten a line's spans into one comparable string.
fn flat(line: &Line) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

fn flats(lines: &[Line]) -> Vec<String> {
    lines.iter().map(flat).collect()
}

fn sel(anchor: (u16, u16), head: (u16, u16)) -> Selection {
    Selection {
        anchor,
        head,
        dragging: false,
    }
}

/// A 6×3 buffer holding three rows of text, for selection extraction tests.
fn sample_buffer() -> Buffer {
    let mut buf = Buffer::empty(Rect::new(0, 0, 6, 3));
    buf.set_string(0, 0, "abcdef", Style::default());
    buf.set_string(0, 1, "ghi", Style::default()); // trailing blanks
    buf.set_string(0, 2, "jklmno", Style::default());
    buf
}

#[test]
fn selection_on_one_row_includes_the_head_cell() {
    // Drag from column 1 to column 3 on row 0 → "bcd" (head inclusive).
    let rows = selection_rows(&sel((1, 0), (3, 0)), 6, 3);
    assert_eq!(rows, vec![(0, 1, 4)]);
    assert_eq!(
        selection_text(&sample_buffer(), &sel((1, 0), (3, 0)), &[]),
        "bcd"
    );
}

#[test]
fn selection_orders_endpoints_regardless_of_drag_direction() {
    // Dragging up-and-left yields the same span as down-and-right.
    let forward = selection_text(&sample_buffer(), &sel((1, 0), (2, 2)), &[]);
    let backward = selection_text(&sample_buffer(), &sel((2, 2), (1, 0)), &[]);
    assert_eq!(forward, backward);
    assert_eq!(forward, "bcdef\nghi\njkl");
}

#[test]
fn selection_trims_trailing_blanks_per_row() {
    // Middle row "ghi" padded to width 6; the blanks must not be copied.
    let text = selection_text(&sample_buffer(), &sel((0, 1), (5, 1)), &[]);
    assert_eq!(text, "ghi");
}

#[test]
fn selection_strips_the_gutter_indent_shared_by_every_row() {
    // What a drag across a Wizard transcript actually copies: the body is
    // inset from the terminal edge, so every row begins with the same
    // spaces. Those are chrome. Nested indent inside the selection is not
    // shared, so it stays.
    let mut buf = Buffer::empty(Rect::new(0, 0, 10, 3));
    buf.set_string(0, 0, "  hello", Style::default());
    buf.set_string(0, 1, "  world", Style::default());
    buf.set_string(0, 2, "    nest", Style::default());
    assert_eq!(
        selection_text(&buf, &sel((0, 0), (9, 2)), &[]),
        "hello\nworld\n  nest"
    );
}

#[test]
fn selection_keeps_indent_that_is_not_shared() {
    // A drag that starts on the text, not the gutter, has no shared
    // leading spaces and must not eat indent that belongs to the content.
    let mut buf = Buffer::empty(Rect::new(0, 0, 8, 2));
    buf.set_string(0, 0, "hello", Style::default());
    buf.set_string(0, 1, "  world", Style::default());
    assert_eq!(
        selection_text(&buf, &sel((0, 0), (7, 1)), &[]),
        "hello\n  world"
    );
}

#[test]
fn a_one_line_selection_keeps_its_own_indent() {
    // Nothing to compare against, so the spaces might be code. Leave them.
    let mut buf = Buffer::empty(Rect::new(0, 0, 10, 1));
    buf.set_string(0, 0, "    foo()", Style::default());
    assert_eq!(selection_text(&buf, &sel((0, 0), (9, 0)), &[]), "    foo()");
}

#[test]
fn selection_does_not_grow_a_gutter_on_continuation_rows() {
    // Drag starts on the text, not column 0. Stream selection still
    // covers column 0 on every row below, which used to paste as a
    // left margin the first line did not have.
    let mut buf = Buffer::empty(Rect::new(0, 0, 10, 2));
    buf.set_string(0, 0, "  hello", Style::default());
    buf.set_string(0, 1, "  world", Style::default());
    assert_eq!(
        selection_text(&buf, &sel((2, 0), (9, 1)), &[]),
        "hello\nworld"
    );
}

#[test]
fn selection_clamps_to_buffer_bounds() {
    // Head past the right/bottom edge stays within the grid.
    let rows = selection_rows(&sel((0, 0), (99, 99)), 6, 3);
    assert_eq!(rows, vec![(0, 0, 6), (1, 0, 6), (2, 0, 6)]);
}

#[test]
fn word_selection_covers_the_identifier_under_the_cursor() {
    let mut buf = Buffer::empty(Rect::new(0, 0, 16, 1));
    buf.set_string(0, 0, "  copy_to_clip x", Style::default());
    let sel = word_selection(&buf, 4, 0).expect("word");
    assert_eq!(selection_text(&buf, &sel, &[]), "copy_to_clip");
    let punct = word_selection(&buf, 0, 0).expect("space");
    assert_eq!(punct.anchor, punct.head);
}

#[test]
fn line_selection_skips_the_gutter_and_trailing_blanks() {
    let mut buf = Buffer::empty(Rect::new(0, 0, 12, 1));
    buf.set_string(0, 0, "  hello   ", Style::default());
    let sel = line_selection(&buf, 0, &[(0, 2)]).expect("line");
    assert_eq!(selection_text(&buf, &sel, &[(0, 2)]), "hello");
}

#[test]
fn selection_highlight_uses_reverse_on_house_cells() {
    let _pinned = theme::pin(theme::minimal());
    let mut buf = Buffer::empty(Rect::new(0, 0, 4, 1));
    buf.set_string(0, 0, "abcd", Style::default());
    paint_selection(&mut buf, &sel((0, 0), (2, 0)), Rect::new(0, 0, 4, 1));
    assert!(
        buf.cell(Position::new(1, 0))
            .unwrap()
            .modifier
            .contains(Modifier::REVERSED),
        "house Reset-on-Reset still uses reverse video"
    );
    assert!(
        !buf.cell(Position::new(3, 0))
            .unwrap()
            .modifier
            .contains(Modifier::REVERSED),
        "cells outside the selection stay put"
    );
}

#[test]
fn selection_highlight_swaps_painted_backgrounds() {
    // Grok paints BgBase on every cell and leaves fg Reset. Reverse of
    // that is terminal-defined and often invisible over SSH.
    let _pinned = theme::pin(theme::load("grok").expect("grok theme").into());
    let mut buf = Buffer::empty(Rect::new(0, 0, 4, 1));
    buf.set_string(
        0,
        0,
        "abcd",
        Style::default().bg(theme::color(Token::BgBase)),
    );
    paint_selection(&mut buf, &sel((0, 0), (2, 0)), Rect::new(0, 0, 4, 1));
    let cell = buf.cell(Position::new(1, 0)).unwrap();
    assert_eq!(cell.fg, theme::color(Token::BgBase));
    assert_eq!(cell.bg, theme::color(Token::Muted));
    assert!(
        !cell.modifier.contains(Modifier::REVERSED),
        "painted cells swap colors instead of reversing"
    );
}

#[test]
fn indeterminate_bar_fills_width_and_animates() {
    let a = flat(&indeterminate_bar(20, 0));
    let b = flat(&indeterminate_bar(20, 7));
    assert_eq!(a.chars().count(), 20, "bar spans the full width");
    assert!(a.contains('█') && a.contains('░'), "has lit and dim cells");
    assert_ne!(a, b, "the lit window moves with the tick");
}

#[test]
fn highlight_diff_uses_red_and_green() {
    // /diff must paint conventional red deletions / green additions so
    // the sidebar is readable at a glance (not grayscale monochrome).
    // The theme is pinned to this thread so the assertion is about the
    // default palette rather than about whatever `WIZARD_THEME` says on
    // the machine running the suite.
    let _pinned = theme::pin(theme::minimal());
    let text = highlight_diff(
        "diff --git a/a.txt b/a.txt\n\
         --- a/a.txt\n\
         +++ b/a.txt\n\
         @@ -1,2 +1,2 @@\n\
          context\n\
         -old\n\
         +new\n",
    );
    let styles: Vec<(String, Style)> = text
        .lines
        .iter()
        .map(|line| {
            let content = line
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>();
            let style = line.spans.first().map(|s| s.style).unwrap_or_default();
            (content, style)
        })
        .collect();

    assert_eq!(
        styles.iter().find(|(c, _)| c == "-old").map(|(_, s)| *s),
        Some(Style::default().fg(Color::Red)),
        "deletions are red"
    );
    assert_eq!(
        styles.iter().find(|(c, _)| c == "+new").map(|(_, s)| *s),
        Some(Style::default().fg(Color::Green)),
        "additions are green"
    );
    // ...and red/green are what the *theme* said, not a literal in the
    // renderer: swapping the theme swaps the sidebar.
    assert_eq!(
        theme::minimal().color(Token::DiffDel),
        Color::Red,
        "the default theme owns the deletion color"
    );
    assert_eq!(theme::minimal().color(Token::DiffAdd), Color::Green);
    // The four header rows collapse into the file's name, once.
    let meta = theme::style(Token::DiffMeta).add_modifier(Modifier::BOLD);
    assert_eq!(
        styles.iter().find(|(c, _)| c == "a.txt").map(|(_, s)| *s),
        Some(meta),
    );
    assert!(
        !styles.iter().any(|(c, _)| {
            c.starts_with("--- ") || c.starts_with("+++ ") || c.starts_with("diff ")
        }),
        "{styles:?}"
    );
    assert_eq!(
        styles
            .iter()
            .find(|(c, _)| c.starts_with("@@"))
            .map(|(_, s)| *s),
        Some(theme::style(Token::DiffHunk)),
    );
}

#[test]
fn cwd_keeps_short_path_intact() {
    let p = std::path::Path::new("/srv/app");
    assert_eq!(format_cwd_from(p, None, 32), "/srv/app");
}

#[test]
fn cwd_drops_leading_components_keeping_leaf() {
    let p = std::path::Path::new("/home/user/projects/ai/wizard");
    // Narrow budget forces dropping leading parts but keeps the leaf.
    let out = format_cwd_from(p, None, 14);
    assert!(out.starts_with('…'), "expected ellipsis prefix, got {out}");
    assert!(out.ends_with("wizard"), "expected leaf kept, got {out}");
    assert!(out.width() <= 14, "expected within budget, got {out}");
}

#[test]
fn cwd_abbreviates_home() {
    let home = std::path::Path::new("/home/user");
    let p = std::path::Path::new("/home/user/projects/ai");
    assert_eq!(format_cwd_from(p, Some(home), 32), "~/projects/ai");
}

#[test]
fn wrap_breaks_at_word_boundaries() {
    let lines = wrap_lines(Text::from(Line::raw("the quick brown fox")), 10);
    assert_eq!(flats(&lines), ["the quick", "brown fox"]);
}

#[test]
fn wrap_moves_whole_word_instead_of_splitting() {
    // The recorded defect: "one occurrence" split as "on / e occurrence".
    let lines = wrap_lines(Text::from(Line::raw("one occurrence")), 12);
    assert_eq!(flats(&lines), ["one", "occurrence"]);
}

#[test]
fn wrap_hard_splits_word_longer_than_width() {
    let lines = wrap_lines(Text::from(Line::raw("abcdefghijkl")), 5);
    assert_eq!(flats(&lines), ["abcde", "fghij", "kl"]);
}

#[test]
fn wrap_continuations_keep_hanging_indent() {
    let line = Line::from(vec![
        Span::styled("· ", accent()),
        Span::raw("alpha beta gamma"),
    ]);
    let lines = wrap_lines(Text::from(line), 9);
    assert_eq!(flats(&lines), ["· alpha", "  beta", "  gamma"]);
    // The marker keeps its accent style; continuations stay raw.
    assert_eq!(lines[0].spans[0].style, accent());
}

#[test]
fn wrap_keeps_styles_across_span_boundary_in_one_word() {
    // Deliberately literal colors, not tokens: the claim under test is
    // that the wrapper preserves *whatever* styles it was handed, so two
    // arbitrary and obviously different ones make a sharper assertion
    // than any pair the theme could supply.
    let red = Style::default().fg(Color::Red);
    let blue = Style::default().fg(Color::Blue);
    // "main.py" is one word spanning two styled spans: it must move to
    // the next line whole, with both styles intact.
    let line = Line::from(vec![
        Span::styled("run ma", red),
        Span::styled("in.py", blue),
    ]);
    let lines = wrap_lines(Text::from(line), 7);
    assert_eq!(flats(&lines), ["run", "main.py"]);
    assert_eq!(lines[1].spans[0].content.as_ref(), "ma");
    assert_eq!(lines[1].spans[0].style, red);
    assert_eq!(lines[1].spans[1].content.as_ref(), "in.py");
    assert_eq!(lines[1].spans[1].style, blue);
}

#[test]
fn wrap_wide_chars_never_exceed_width() {
    let lines = wrap_lines(Text::from(Line::raw("日本語のテスト")), 5);
    assert_eq!(flats(&lines), ["日本", "語の", "テス", "ト"]);
    for line in &lines {
        assert!(line.width() <= 5);
    }
}

#[test]
fn wrap_keeps_combining_marks_with_base_char() {
    let lines = wrap_lines(Text::from(Line::raw("e\u{301}".repeat(5))), 3);
    assert_eq!(flats(&lines), ["e\u{301}".repeat(3), "e\u{301}".repeat(2)]);
}

#[test]
fn wrap_leaves_short_lines_untouched() {
    let line = Line::from(vec![Span::styled("❯ ", dim()), Span::raw("hi")]);
    let lines = wrap_lines(Text::from(line.clone()), 10);
    assert_eq!(lines, vec![line]);
}

#[test]
fn hanging_indent_detects_gutter_marks() {
    assert_eq!(hanging_indent(&Line::raw("❯ hello")), 2);
    assert_eq!(hanging_indent(&Line::raw("· hello")), 2);
    assert_eq!(hanging_indent(&Line::raw("✓ tool")), 2);
    assert_eq!(hanging_indent(&Line::raw("  • item")), 4);
    assert_eq!(hanging_indent(&Line::raw("  plain")), 2);
    assert_eq!(hanging_indent(&Line::raw("plain text")), 0);
    // A dim rule is not a mark (wider than two columns of glyphs).
    assert_eq!(hanging_indent(&Line::raw("────────")), 0);
}

/// The console rule never draws wider than the rule it is.
///
/// It clamped its arithmetic to `width` but pushed the label whole, so on a
/// pane narrower than the label the line overflowed: the trailing fill was
/// lost and the command name was cut by the frame with no ellipsis. The
/// label is " ▶ stdin → " plus up to 48 columns of command, about 59, so
/// anything under roughly 60 columns hit it. This rule is what tells the
/// user Enter now types into a command instead of the agent, which makes it
/// the worst line in the composer to render unreadably.
///
/// The audit that found this could not reproduce it on screen — it needs a
/// live console, and opening one costs a paid agent turn. It is a pure
/// function, so it does not need one.
#[test]
fn the_console_rule_never_overflows_the_width_it_is_given() {
    let long = "cargo test --locked --features native -- --nocapture --test-threads=1";
    for width in [20u16, 40, 59, 60, 80, 120] {
        let rule = console_rule(long, width);
        assert!(
            rule.width() <= width as usize,
            "a {width}-column rule drew {} columns: {rule:?}",
            rule.width()
        );
    }

    // Still a rule, not just a label: at a comfortable width it fills.
    assert_eq!(console_rule("ls", 80).width(), 80);
    // And it still says what it is for.
    let text: String = console_rule("ls", 80)
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert!(text.contains("stdin"), "{text}");
}

#[test]
fn truncate_line_cuts_with_dim_ellipsis() {
    // Literal, for the same reason as the wrap tests above: this asserts
    // that a span's own style survives truncation, whatever it is.
    let red = Style::default().fg(Color::Red);
    let line = Line::from(vec![Span::raw("abc"), Span::styled("defgh", red)]);
    let out = truncate_line(line, 5);
    assert_eq!(flat(&out), "abcd…");
    assert_eq!(out.spans[1].style, red);
    assert_eq!(out.spans.last().unwrap().content.as_ref(), "…");
    assert_eq!(out.spans.last().unwrap().style, dim());
    assert!(out.width() <= 5);
}

#[test]
fn truncate_line_leaves_fitting_lines_alone() {
    let line = Line::raw("short");
    assert_eq!(truncate_line(line.clone(), 10), line);
}

const TABLE: &str = "| Field | Value |\n|---|---:|\n\
                     | **Capital** | N'Djamena |\n| Population | ~19-20 million |\n";

fn span_style(line: &Line, content: &str) -> Style {
    line.spans
        .iter()
        .find(|span| span.content.as_ref() == content)
        .unwrap_or_else(|| panic!("no span {content:?} in {:?}", flat(line)))
        .style
}

#[test]
fn markdown_table_renders_as_aligned_grid() {
    let text = render_markdown_at(TABLE, usize::MAX);
    assert_eq!(
        flats(&text.lines),
        vec![
            "Field      │          Value",
            "───────────┼───────────────",
            "Capital    │      N'Djamena",
            "Population │ ~19-20 million",
        ]
    );
}

#[test]
fn markdown_table_right_aligns_column_by_padding_left() {
    let text = render_markdown_at("| a | num |\n|---|--:|\n| b | 7 |\n", usize::MAX);
    assert_eq!(flats(&text.lines), vec!["a │ num", "──┼────", "b │   7"]);
}

#[test]
fn markdown_table_preserves_inline_styling() {
    let text = render_markdown_at(TABLE, usize::MAX);
    let header = span_style(&text.lines[0], "Field");
    assert!(header.add_modifier.contains(Modifier::BOLD));
    let strong = span_style(&text.lines[2], "Capital");
    assert!(strong.add_modifier.contains(Modifier::BOLD));
    let plain = span_style(&text.lines[2], "N'Djamena");
    assert!(!plain.add_modifier.contains(Modifier::BOLD));
}

#[test]
fn markdown_table_pads_ragged_rows_as_empty_cells() {
    let text = render_markdown_at("| a | b | c |\n|---|---|---|\n| long-cell |\n", usize::MAX);
    let widths: Vec<usize> = flats(&text.lines)
        .iter()
        .map(|line| UnicodeWidthStr::width(line.as_str()))
        .collect();
    assert_eq!(widths, vec![17, 17, 17]);
}

#[test]
fn markdown_table_fits_narrow_width_without_soft_wrap_mid_grid() {
    // Natural width of this table is well over 40 columns. Laid out into 40,
    // every row (and the header rule) must fit, and every data row still
    // carries a `│` separator — soft-wrapping the whole line would drop
    // columns off the right edge or split mid-cell without a rule.
    let md = "\
| Stage | What people actually do | What it buys |
|---|---|---|
| Pretraining (from scratch with latents) | Almost nobody at scale | Deep internalization |
| Fine-tune a pretrained LM | Standard path (Coconut, ICoT) | Reuse English fluency |
";
    let text = render_markdown_at(md, 40);
    let flats = flats(&text.lines);
    assert!(!flats.is_empty());
    for line in &flats {
        assert!(
            UnicodeWidthStr::width(line.as_str()) <= 40,
            "row exceeds budget: {line:?} (width {})",
            UnicodeWidthStr::width(line.as_str())
        );
    }
    // Header rule still present and within budget.
    assert!(
        flats.iter().any(|line| line.contains('┼')),
        "expected a header rule, got {flats:?}"
    );
    // At least one body line still has a column separator — the grid
    // survived the shrink, it wasn't flattened into free text.
    let body: Vec<_> = flats.iter().filter(|line| line.contains('│')).collect();
    assert!(
        body.len() >= 2,
        "expected header + body with separators, got {flats:?}"
    );
}

#[test]
fn markdown_table_wraps_long_cells_inside_columns() {
    // A two-column table forced into a tight budget: the long left cell
    // must wrap onto a second line under its own column, not push past │.
    let md = "| left | right |\n|---|---|\n| wordy phrase here | ok |\n";
    let text = render_markdown_at(md, 20);
    let flats = flats(&text.lines);
    // Natural single-line would be ~"wordy phrase here │ ok" (~22+); under
    // 20 the first body row wraps.
    let body: Vec<_> = flats
        .iter()
        .filter(|line| line.contains('│') && !line.contains("left"))
        .cloned()
        .collect();
    assert!(
        body.len() >= 2,
        "expected multi-line wrapped body, got {flats:?}"
    );
    for line in &body {
        assert!(
            UnicodeWidthStr::width(line.as_str()) <= 20,
            "wrapped body line too wide: {line:?}"
        );
    }
}

#[test]
fn fit_column_widths_never_goes_below_one() {
    let mut widths = vec![10, 10, 10];
    fit_column_widths(&mut widths, 2);
    assert_eq!(widths, vec![1, 1, 1]);
    assert_eq!(widths.iter().sum::<usize>(), 3);
}

#[test]
fn fit_column_widths_trims_the_widest_first() {
    let mut widths = vec![20, 5, 5];
    fit_column_widths(&mut widths, 20);
    assert_eq!(widths.iter().sum::<usize>(), 20);
    assert!(widths[0] >= widths[1] && widths[0] >= widths[2]);
    assert!(widths.iter().all(|&w| w >= 1));
}

#[test]
fn latex_to_unicode_maps_greek_and_scripts() {
    assert_eq!(latex_to_unicode(r"\alpha + \beta = \gamma"), "α + β = γ");
    assert_eq!(latex_to_unicode(r"x^2 + y_1"), "x² + y₁");
    assert_eq!(latex_to_unicode(r"\mathbb{E}[X]"), "𝔼[X]");
    assert_eq!(latex_to_unicode(r"\mathbb{R}"), "ℝ");
    // \frac becomes a slash form — readable in one terminal row.
    assert_eq!(latex_to_unicode(r"\frac{1}{2}"), "1/2");
    assert_eq!(latex_to_unicode(r"\sqrt{x}"), "√x");
    // Whole-command matching: `\in` must not nibble `\infty`.
    assert_eq!(latex_to_unicode(r"x \in \mathbb{R}"), "x ∈ ℝ");
    assert_eq!(latex_to_unicode(r"\infty"), "∞");
    assert_eq!(latex_to_unicode(r"A^\top"), "Aᵀ");
}

#[test]
fn latex_to_unicode_strips_font_wrappers() {
    // The screenshot case: nested \mathrm/\mathbf/\boldsymbol + blackboard E.
    let tex = r"\mathrm{Cov}(\mathbf{z}_{\mathrm{pos}}, \mathbf{z}_{\mathrm{neg}}) = \mathbb{E}[(\mathbf{z}_{\mathrm{pos}} - \boldsymbol{\mu}_{\mathrm{pos}})(\mathbf{z}_{\mathrm{neg}} - \boldsymbol{\mu}_{\mathrm{neg}})^\top]";
    let out = latex_to_unicode(tex);
    // No raw TeX commands or `$` left.
    assert!(!out.contains('\\'), "still has backslash: {out}");
    assert!(!out.contains("mathrm"), "{out}");
    assert!(!out.contains("mathbf"), "{out}");
    assert!(out.contains('𝔼'), "expected 𝔼 in {out}");
    assert!(
        out.contains('μ') || out.contains("mu"),
        "expected mu in {out}"
    );
    // Transpose: superscript T preferred over caret+⊤.
    assert!(
        out.contains('ᵀ') || out.contains('⊤') || out.contains("^T"),
        "expected transpose in {out}"
    );
    // pos fully maps to subscripts; neg has no subscript-g so falls back flat.
    assert!(
        out.contains("ₚₒₛ") || out.contains("_pos"),
        "expected pos subscript form in {out}"
    );
    assert!(
        out.contains("Cov"),
        "expected Cov operator name kept, got {out}"
    );
}

#[test]
fn markdown_inline_math_renders_as_unicode() {
    let text = render_markdown_at(r"See $\alpha + \beta$ for details.", usize::MAX);
    let joined = flats(&text.lines).join("\n");
    assert!(
        joined.contains("α + β"),
        "expected unicode math, got {joined:?}"
    );
    assert!(
        !joined.contains('$') && !joined.contains('\\'),
        "raw delimiters leaked: {joined:?}"
    );
}

#[test]
fn markdown_display_math_is_indented_on_own_line() {
    let text = render_markdown_at("before\n\n$$\\sum_{i=1}^{n} x_i$$\n\nafter\n", usize::MAX);
    let flats = flats(&text.lines);
    // Display math is its own indented line, not jammed into prose.
    assert!(
        flats
            .iter()
            .any(|line| line.contains('∑') && line.starts_with("  ")),
        "expected indented display math, got {flats:?}"
    );
    assert!(
        flats.iter().any(|line| line.contains("before")),
        "{flats:?}"
    );
    assert!(flats.iter().any(|line| line.contains("after")), "{flats:?}");
}

fn cs(text: &str) -> Vec<char> {
    text.chars().collect()
}

#[test]
fn composer_wrap_keeps_short_lines_whole() {
    assert_eq!(wrap_rows(&cs("hello"), 10), vec![(0, 5)]);
    assert_eq!(wrap_rows(&cs(""), 10), vec![(0, 0)]);
}

#[test]
fn composer_wrap_splits_long_lines_at_the_budget() {
    // "abcdef" at 3 columns: two full rows; at 4: 4 + 2.
    assert_eq!(wrap_rows(&cs("abcdef"), 3), vec![(0, 3), (3, 6)]);
    assert_eq!(wrap_rows(&cs("abcdef"), 4), vec![(0, 4), (4, 6)]);
}

#[test]
fn composer_wrap_respects_hard_breaks_and_trailing_newline() {
    // The '\n' belongs to no row; a trailing one yields an empty last row.
    assert_eq!(wrap_rows(&cs("ab\ncd"), 10), vec![(0, 2), (3, 5)]);
    assert_eq!(wrap_rows(&cs("ab\n"), 10), vec![(0, 2), (3, 3)]);
    assert_eq!(wrap_rows(&cs("a\n\nb"), 10), vec![(0, 1), (2, 2), (3, 4)]);
}

#[test]
fn composer_wrap_never_splits_a_wide_char() {
    // '你' is 2 columns; at budget 3 it doesn't fit after "ab" and moves
    // whole to the next row.
    assert_eq!(wrap_rows(&cs("ab你c"), 3), vec![(0, 2), (2, 4)]);
}

#[test]
fn composer_cursor_maps_through_soft_wraps() {
    let rows = wrap_rows(&cs("abcdef"), 3); // (0,3) (3,6)
    assert_eq!(cursor_visual(&rows, 2), (0, 2));
    // Exactly on the wrap boundary: start of the next visual row.
    assert_eq!(cursor_visual(&rows, 3), (1, 0));
    // End of text: end of the last row.
    assert_eq!(cursor_visual(&rows, 6), (1, 3));
}

#[test]
fn composer_cursor_stays_on_its_row_at_hard_breaks() {
    let rows = wrap_rows(&cs("ab\ncd"), 10); // (0,2) (3,5)
    // On the '\n' itself: end of the row before it.
    assert_eq!(cursor_visual(&rows, 2), (0, 2));
    assert_eq!(cursor_visual(&rows, 3), (1, 0));
    assert_eq!(cursor_visual(&rows, 5), (1, 2));
}

#[test]
fn composer_cursor_on_empty_input_is_origin() {
    let rows = wrap_rows(&cs(""), 10);
    assert_eq!(cursor_visual(&rows, 0), (0, 0));
}

#[test]
fn long_input_soft_wraps_instead_of_scrolling() {
    let mut app = App::new(crate::config::Config::default());
    app.input = "x".repeat(100);
    app.cursor = 100;

    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|frame| draw(frame, &app)).unwrap();

    // 80 columns leave 76 for text, so 100 chars fill one row and wrap 24
    // onto a continuation row. The composer sits above the status line:
    // rule (19), two text rows (20–21), rule (22).
    let buffer = terminal.backend().buffer().clone();
    let row = |y: u16| -> String { (0..80).map(|x| buffer[(x, y)].symbol()).collect::<String>() };
    assert_eq!(row(20).trim_end(), format!(" ❯ {}", "x".repeat(76)));
    assert_eq!(row(21).trim_end(), format!("   {}", "x".repeat(24)));
    // The caret follows onto the wrapped row instead of the old
    // horizontal scroll keeping everything on one line.
    let cursor = terminal.get_cursor_position().unwrap();
    assert_eq!((cursor.x, cursor.y), (3 + 24, 21));
}

/// Render `app` at 80x24 and return the screen as one string per row.
fn render(app: &App) -> Vec<String> {
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|frame| draw(frame, app)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    (0..24)
        .map(|y| {
            (0..80)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

fn app_with_run() -> App {
    let mut app = App::new(crate::config::Config::default());
    app.handle_agent_event(crate::agent::AgentEvent::SubagentRunStarted {
        run: 1,
        bg: Some(1),
        name: "researcher".to_string(),
        task: "map the auth flow".to_string(),
    });
    app
}

#[test]
fn the_rail_paints_a_dot_per_subagent_under_the_composer() {
    let mut app = app_with_run();
    app.handle_agent_event(crate::agent::AgentEvent::SubagentRunToolStarted {
        run: 1,
        name: "read_file".to_string(),
        args: serde_json::json!({}),
    });

    let rows = render(&app);
    // The rail sits between the composer and the status bar (row 23).
    let rail = rows
        .iter()
        .find(|row| row.contains("researcher"))
        .expect("the rail shows the run");
    assert!(rail.contains("read_file"), "shows what it is doing: {rail}");
    // Unread work is badged, so you can tell it moved while you looked away.
    assert!(rail.contains("+1"), "shows the unread badge: {rail}");
}

#[test]
fn no_subagents_means_no_rail_and_no_lost_rows() {
    let bare = App::new(crate::config::Config::default());
    let with_run = app_with_run();
    // The rail costs nothing until there is something to show, and then
    // takes exactly the one row it needs.
    assert_eq!(rail_height(&bare), 0);
    assert_eq!(rail_height(&with_run), 1);
}

#[test]
fn todo_band_sits_above_the_composer_without_covering_chat() {
    use crate::tools::todo::{TodoItem, TodoStatus};

    let mut app = App::new(crate::config::Config::default());
    app.show_todos = true;
    app.todos = vec![
        TodoItem {
            content: "done already".to_string(),
            status: TodoStatus::Completed,
        },
        TodoItem {
            content: "working on this".to_string(),
            status: TodoStatus::InProgress,
        },
        TodoItem {
            content: "later".to_string(),
            status: TodoStatus::Pending,
        },
    ];
    app.transcript
        .user("full-width chat line".to_string(), Vec::new());

    let rows = render(&app);
    let joined = rows.join("\n");
    assert!(
        joined.contains("todos 1/3"),
        "band title shows progress: {joined}"
    );
    assert!(
        joined.contains("working on this"),
        "current item is visible: {joined}"
    );
    assert!(
        rows.iter().any(|r| r.contains("full-width chat line")),
        "transcript still shows above the band: {joined}"
    );
    // Chat keeps full width: the diff sidebar uses a LEFT border on the
    // right 40%, so a pure todo-sidebar would leave an empty right column.
    // With the band, the title/items span the full terminal width.
    let title = rows
        .iter()
        .find(|r| r.contains("todos 1/3"))
        .expect("title row");
    // Title is left-anchored (not in a right-hand 40% pane starting ~col 48).
    let title_col = title.find("todos 1/3").expect("title text");
    assert!(
        title_col < 20,
        "todo band is left-anchored above the input, not a right sidebar: col={title_col} row={title:?}"
    );

    // The band owns layout rows: chat text must appear *above* the todo
    // title, never on the same rows (which would mean the panel covered it).
    let chat_row = rows
        .iter()
        .position(|r| r.contains("full-width chat line"))
        .expect("chat row");
    let todo_row = rows
        .iter()
        .position(|r| r.contains("todos 1/3"))
        .expect("todo title row");
    assert!(
        chat_row < todo_row,
        "chat text must sit above the todo band (chat={chat_row}, todo={todo_row}): {joined}"
    );

    // And regions must reserve non-zero height for the band while chat
    // still gets at least one row.
    let Regions {
        body: main,
        todo,
        composer: input,
        ..
    } = regions(&app, ratatui::layout::Rect::new(0, 0, 80, 24));
    assert!(main.height >= 1, "transcript keeps a row");
    assert!(todo.height >= 3, "todo band is reserved: {}", todo.height);
    assert!(
        todo.y + todo.height == input.y,
        "todo band sits directly above the composer"
    );
}

/// Adversarial: a startup notice raised before the first message (a theme
/// name that would not load, a config that did not parse) is pushed to the
/// transcript, and the transcript is not drawn while the welcome card is
/// up. `WIZARD_THEME=solarised wizard` therefore opened on the default
/// theme with no indication the name was wrong, and the notice only became
/// The mark appears when there is room and yields when there is not.
///
/// The hints under the name are the useful half of this screen — "type a
/// message and press Enter to begin" is what a first-time reader needs. A
/// thirteen-line drawing that pushed them off a short terminal would be a
/// splash actively getting in the way, so the art is conditional and this
/// pins both directions.
#[test]
fn the_empty_state_names_the_session_and_one_hint() {
    fn screen(width: u16, height: u16) -> String {
        let app = App::new(crate::config::Config::default());
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    for (width, height) in [(120, 40), (80, 24), (40, 20)] {
        let screen = screen(width, height);
        let first = screen.lines().nth(1).unwrap_or_default();
        assert!(first.contains("◆ wizard"), "{width}x{height}: {screen}");
        assert!(
            first.contains(env!("CARGO_PKG_VERSION")),
            "{width}x{height}: {screen}"
        );
        assert!(
            screen.contains("type a message"),
            "{width}x{height}: {screen}"
        );
        assert!(screen.contains("model"), "{width}x{height}: {screen}");
        assert!(screen.contains("mode"), "{width}x{height}: {screen}");
        assert!(screen.contains("genie"), "{width}x{height}: {screen}");
        assert!(screen.contains("context"), "{width}x{height}: {screen}");
        assert!(
            !screen.contains("· / lists"),
            "one hint, not a tip row: {width}x{height}: {screen}"
        );
        for absent in ["⣿", "w i z a r d", "sovereign agent", "/model"] {
            assert!(
                !screen.contains(absent),
                "{absent} at {width}x{height}: {screen}"
            );
        }
    }
}

/// The empty-state hook: the starter prompts are muted `❯` rows under the
/// hint, never numbered and never introduced by a key hint; the first-run
/// summary is a dim line after them, not a warning; the ↓ pick is the row
/// that moves.
#[test]
fn starter_prompts_and_the_first_run_summary_sit_on_the_welcome_card() {
    let _skin = crate::skin::pin(crate::skin::Skin::Wizard);
    let mut app = App::new(crate::config::Config::default());
    app.starter_prompts = vec![
        "Explain how this project is put together".to_string(),
        "Review my uncommitted changes".to_string(),
    ];
    app.first_run_summary = Some("saved ~/.wizard/config.toml · /setup changes it".to_string());
    let rows = render(&app);
    let screen = rows.join("\n");
    let hint = rows
        .iter()
        .position(|row| row.contains("type a message"))
        .expect("the hint");
    let first = rows
        .iter()
        .position(|row| row.contains("Explain how this project is put together"))
        .expect("first prompt");
    assert!(first < hint, "prompts sit above the hint: {screen}");
    assert_eq!(
        rows[first].trim(),
        "❯ Explain how this project is put together",
        "{screen}"
    );
    assert_eq!(rows[first + 1].trim(), "❯ Review my uncommitted changes");
    assert_eq!(rows[hint + 1].trim(), "");
    assert_eq!(
        rows[hint + 2].trim(),
        "saved ~/.wizard/config.toml · /setup changes it",
        "the summary comes after the hint, apart: {screen}"
    );
    assert!(!screen.contains("1  Explain"), "no digits: {screen}");
    assert!(!screen.contains("pick one"), "no key hint: {screen}");
    assert!(
        !screen.contains("⚠ saved"),
        "the summary is not a warning: {screen}"
    );
    assert!(app.welcome_visible());

    // A failed probe is one line with the remedy on it, and what the first
    // run could not settle is one more.
    app.provider_health_error = Some("not signed in to xAI; run `wizard --login xai` first".into());
    app.first_run_notice =
        Some("api.x.ai did not answer in 3 seconds; the key is saved unchecked".into());
    let screen = render(&app).join("\n");
    assert!(
        screen.contains("⚠ not signed in to xAI: /login xai"),
        "{screen}"
    );
    assert!(screen.contains("⚠ api.x.ai did not answer"), "{screen}");
    // The same fact from the probe and the first run is said once.
    app.provider_health_error = Some("cannot reach https://api.x.ai/v1: refused".into());
    app.first_run_notice = Some("cannot reach api.x.ai; the key is saved unchecked".into());
    let screen = render(&app).join("\n");
    assert_eq!(
        screen.matches("cannot reach api.x.ai").count(),
        1,
        "{screen}"
    );
    app.provider_health_error = None;
    app.first_run_notice = None;

    // No prompts, no hook: the title, the four-row block, and one hint.
    app.starter_prompts.clear();
    app.first_run_summary = None;
    let rows = render(&app);
    let text: Vec<&str> = rows
        .iter()
        .map(|row| row.trim())
        .filter(|row| !row.is_empty())
        .collect();
    assert_eq!(
        text[0],
        format!("◆ wizard {}", env!("CARGO_PKG_VERSION")),
        "{rows:?}"
    );
    assert!(text.iter().any(|row| row.starts_with("model")), "{rows:?}");
    assert!(text.iter().any(|row| row.starts_with("mode")), "{rows:?}");
    assert!(text.iter().any(|row| row.starts_with("dir")), "{rows:?}");
    assert!(
        text.iter().any(|row| row.starts_with("context")),
        "{rows:?}"
    );
    assert!(text.contains(&"type a message"), "{rows:?}");
}

/// At 40 columns every card row that does not fit ends in `…` instead of
/// clipping mid-word, and none is wider than the screen.
#[test]
fn the_welcome_card_truncates_with_an_ellipsis_at_forty_columns() {
    let _skin = crate::skin::pin(crate::skin::Skin::Wizard);
    let mut app = App::new(crate::config::Config::default());
    app.starter_prompts = vec!["Explain how this project is put together".to_string()];
    app.first_run_summary = Some("saved ~/.wizard/config.toml · /setup changes it".to_string());
    app.provider_health_error =
        Some("cannot reach https://api.anthropic.com: error sending request for url".into());
    let backend = ratatui::backend::TestBackend::new(40, 20);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|frame| draw(frame, &app)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    let rows: Vec<String> = (0..20)
        .map(|y| {
            (0..40)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect();
    // The probe's line is reduced to the host, so it fits whole.
    assert!(
        rows.iter()
            .any(|row| row.trim() == "⚠ cannot reach api.anthropic.com"),
        "{rows:?}"
    );
    let cut: Vec<&String> = rows
        .iter()
        .filter(|row| row.contains("Explain") || row.contains("saved"))
        .collect();
    assert_eq!(cut.len(), 2, "{rows:?}");
    for row in cut {
        assert!(row.ends_with('…'), "{row:?}");
        assert!(row.chars().count() <= 40, "{row:?}");
    }
}

/// visible after the user's first submission.
#[test]
fn a_startup_notice_is_visible_on_the_welcome_screen() {
    let mut app = App::new(crate::config::Config::default());
    assert!(app.welcome_visible(), "the premise: the card is up");
    app.notice("theme: unknown theme 'solarised'; using minimal");

    let screen = render(&app).join("\n");
    assert!(
        screen.contains("unknown theme 'solarised'"),
        "the notice has to be on the card the user is looking at: {screen}"
    );
    assert!(screen.contains(" wizard "), "still the welcome card");
    // A notice does not start the conversation, so the card stays.
    assert!(app.welcome_visible());

    // Only the newest few, so a noisy start cannot push the card off
    // screen; the oldest is the one that gives way.
    for n in 0..6 {
        app.notice(format!("notice number {n}"));
    }
    let screen = render(&app).join("\n");
    assert!(screen.contains("notice number 5"), "{screen}");
    assert!(!screen.contains("notice number 1"), "{screen}");
    assert!(
        !screen.contains("unknown theme 'solarised'"),
        "the oldest gives way: {screen}"
    );
}

/// Adversarial: the `/diff` sidebar and the dashboard peek panel took
/// `Token::Border`'s color but not the theme's border *glyphs*, so under a
/// theme with heavier chrome every floating layer drew with ═ while these
/// two rules stayed ┃. Rendered rather than asserted structurally, so it
/// is the buffer that decides.
#[test]
fn the_diff_rule_uses_the_themes_border_glyphs() {
    let mut app = App::new(crate::config::Config::default());
    app.diff = Some(crate::app::DiffPane {
        text: "diff --git a/a.txt b/a.txt\n+new line\n".to_string(),
        scroll: 0,
    });

    // Built here rather than loaded by name: the property under test is
    // that the rule takes its glyph from the theme, and every palette that
    // ships draws either rounded or plain borders — both of which are `│`
    // down the side, so a shipped one could not tell the two apart. A
    // theme declaring a border style nothing else uses can.
    let thick = theme::Theme::parse(
        "thick-for-this-test",
        "border = \"thick\"\n",
        &theme::minimal(),
    )
    .expect("a one-key theme over the defaults");
    // Pinned *after* construction: `App::new` installs a theme, which is
    // exactly the interaction the pin is there to survive.
    let _pinned = theme::pin(std::sync::Arc::new(thick));
    let screen = render(&app).join("\n");
    assert!(
        screen.contains('┃'),
        "the sidebar rule must use the theme's border glyph: {screen}"
    );

    // Control: under the default theme the same rule is the plain one, so
    // the assertion above is about the theme and not about the glyph
    // happening to be there.
    let _pinned = theme::pin(theme::minimal());
    let screen = render(&app).join("\n");
    assert!(!screen.contains('║'), "{screen}");
    assert!(screen.contains('│'), "{screen}");
}

/// Every themed border takes the theme's glyphs as well as its color.
/// Two of the eight sites took only the color, and a rendering test can
/// only reach the layers a test can open, so the property is pinned
/// against this file's own source as well.
///
/// Per *block*, not per file: counting the two calls over the whole module
/// and comparing the totals is a check that eight equals eight, which two
/// `.border_type()` calls on one block and none on another satisfy just as
/// well as the property does.
#[test]
fn every_themed_border_also_takes_the_themes_border_type() {
    const SOURCE: &str = include_str!("mod.rs");
    let production = SOURCE
        .split("#[cfg(test)]")
        .next()
        .expect("split always yields a first part");

    // Each site is one builder chain, `Block::…()` through the `;` that
    // ends the statement, so splitting on the constructor and cutting at
    // the first semicolon yields exactly one block per chunk. A chain that
    // ever contains a statement of its own (a closure with a body) would
    // need a real parse; the assertion on the count below is what notices
    // that the scan stopped matching this file.
    let blocks: Vec<&str> = production
        .split("Block::")
        .skip(1)
        .map(|chunk| chunk.split(';').next().unwrap_or(chunk))
        .collect();
    let themed: Vec<&&str> = blocks
        .iter()
        .filter(|block| block.contains(".border_style(theme::style(Token::Border))"))
        .collect();
    assert!(
        themed.len() >= 8,
        "the scan found only {} themed borders in {} blocks; it has stopped \
         matching this file",
        themed.len(),
        blocks.len()
    );
    for block in themed {
        assert!(
            block.contains(".border_type(theme::border_type())"),
            "this border takes the theme's color but not its glyphs; add \
             .border_type(theme::border_type()):\nBlock::{block}"
        );
    }
}

/// Adversarial: the code-block cache holds *styled* lines, and nothing
/// exercised its key. Drop the depth from it and a terminal that just
/// reported 16 colors is served the truecolor render that is already in
/// the map, which is 24-bit escapes printed as literal text.
#[test]
fn the_code_block_cache_is_keyed_by_the_depth_it_was_highlighted_under() {
    // Unique to this test so no other cached block can answer for it.
    let code = "fn code_block_cache_key_probe() -> u8 { 7 }\n";
    let at = |depth| {
        let _pin = theme::pin(std::sync::Arc::new(theme::minimal().with_depth(depth)));
        highlight_code_block("rust", code)
    };

    let truecolor = at(theme::ColorDepth::TrueColor);
    let degraded = at(theme::ColorDepth::Ansi16);
    let widened = at(theme::ColorDepth::TrueColor);

    let colors = |lines: &[Line<'static>]| -> Vec<Color> {
        lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter_map(|span| span.style.fg)
            .collect()
    };
    assert!(
        colors(&truecolor)
            .iter()
            .any(|color| matches!(color, Color::Rgb(..))),
        "the highlighter should emit 24-bit color at full depth"
    );
    for color in colors(&degraded) {
        assert!(
            theme::is_ansi16(color),
            "a cached truecolor render reached a 16-color terminal: {color:?}"
        );
    }
    // And back: the cache must not have been poisoned by the narrow pass.
    assert_eq!(colors(&widened), colors(&truecolor));
}

#[test]
fn attaching_replaces_the_chat_with_the_subagents_own_transcript() {
    let mut app = app_with_run();
    app.transcript
        .user("main conversation".to_string(), Vec::new());
    app.handle_agent_event(crate::agent::AgentEvent::SubagentRunText {
        run: 1,
        text: "the auth flow starts in login.rs".to_string(),
    });
    app.attach_pane(0);

    let rows = render(&app);
    let screen = rows.join("\n");
    // The pane took over: its header names the run, its message is on
    // screen, and the main conversation is not.
    assert!(screen.contains("researcher"), "{screen}");
    assert!(screen.contains("running"), "{screen}");
    assert!(
        screen.contains("the auth flow starts in login.rs"),
        "{screen}"
    );
    assert!(!screen.contains("main conversation"), "{screen}");
    // And there is a way back.
    assert!(screen.contains("esc back"), "{screen}");
}

fn todo_items(n: usize) -> Vec<crate::tools::todo::TodoItem> {
    use crate::tools::todo::{TodoItem, TodoStatus};
    (0..n)
        .map(|i| TodoItem {
            content: format!("item {i}"),
            status: TodoStatus::Pending,
        })
        .collect()
}

#[test]
fn todo_band_gives_way_on_a_tiny_terminal() {
    let mut app = App::new(crate::config::Config::default());
    app.show_todos = true;
    app.todos = todo_items(3);
    let Regions {
        body: main,
        todo,
        composer: input,
        footer: status,
        ..
    } = regions(&app, ratatui::layout::Rect::new(0, 0, 80, 6));
    assert_eq!(todo.height, 0, "no room for the band without starving chat");
    assert!(main.height >= 1, "the transcript keeps a row");
    assert_eq!(status.height, 1);
    assert!(input.height >= 3);
}

#[test]
fn todo_band_caps_its_height_however_long_the_list() {
    let mut app = App::new(crate::config::Config::default());
    app.show_todos = true;
    app.todos = todo_items(30);
    let Regions {
        body: main, todo, ..
    } = regions(&app, ratatui::layout::Rect::new(0, 0, 80, 40));
    assert_eq!(todo.height, 12, "a long list cannot swallow the screen");
    assert!(main.height >= 1);

    // An empty list still shows the band frame: title plus one row.
    app.todos.clear();
    let Regions { todo, .. } = regions(&app, ratatui::layout::Rect::new(0, 0, 80, 40));
    assert_eq!(todo.height, 3);
}

#[test]
fn composer_growth_is_capped_and_the_status_bar_survives() {
    let mut app = App::new(crate::config::Config::default());
    app.input = "line\n".repeat(20);
    app.cursor = app.input.chars().count();
    let Regions {
        body: main,
        composer: input,
        footer: status,
        ..
    } = regions(&app, ratatui::layout::Rect::new(0, 0, 80, 24));
    assert_eq!(input.height, 12, "ten text rows plus the two rules");
    assert!(main.height >= 1, "the transcript is never squeezed out");
    assert_eq!(status.height, 1);
}

#[test]
fn rail_stays_one_row_until_it_is_focused() {
    let mut app = App::new(crate::config::Config::default());
    for run in 0..12u64 {
        app.handle_agent_event(crate::agent::AgentEvent::SubagentRunStarted {
            run,
            bg: None,
            name: format!("agent{run}"),
            task: "t".to_string(),
        });
        // Unfocused: one summary, however many runs. The list must not eat
        // the transcript just because work was delegated.
        assert_eq!(rail_height(&app), 1, "{} panes, unfocused", app.panes.len());
    }
    assert!(app.focus_rail());
    assert_eq!(
        rail_height(&app),
        6,
        "focused list is capped at five plus overflow"
    );
    app.attach_pane(0);
    assert_eq!(
        rail_height(&app),
        1,
        "inside a run the siblings collapse back to the summary"
    );
}

#[test]
fn a_masked_api_key_never_reaches_the_screen() {
    let mut app = App::new(crate::config::Config::default());
    app.web_key_backend = Some("brave".to_string());
    app.input = "sk-supersecret".to_string();
    app.cursor = app.input.chars().count();
    let rows = render(&app);
    let screen = rows.join("\n");
    assert!(!screen.contains("sk-supersecret"), "{screen}");
    assert!(!screen.contains("supersecret"), "{screen}");
    assert!(
        screen.contains(&"•".repeat("sk-supersecret".len())),
        "each typed char shows as a bullet: {screen}"
    );
}
/// Text that has, in one renderer or another, been the thing that turned
/// a frame into a crash: characters two columns wide, characters zero
/// columns wide, grapheme clusters that are neither, escape sequences the
/// terminal would have eaten, a token with nowhere to wrap, and nothing at
/// all.
fn adversarial_strings() -> Vec<String> {
    vec![
        String::new(),
        " ".to_string(),
        "\n\n\n".to_string(),
        // Two columns per char: any renderer that budgets in `chars()` and
        // slices in columns (or the reverse) misaligns here first.
        "宽字符测试".repeat(8),
        // One grapheme, four scalars, several joiners.
        "👨‍👩‍👧‍👦 family".to_string(),
        // Combining marks stack onto the previous cell and are zero-width.
        "e\u{301}\u{301}\u{301}\u{327} combining".to_string(),
        // A zero-width space and a BOM in the middle of a line.
        "before\u{200b}\u{feff}after".to_string(),
        // Raw ANSI, including a clear-screen, arriving as tool output.
        "\x1b[31mred\x1b[0m \x1b[2J \x1b[1;1H done".to_string(),
        // Control characters and a lone carriage return.
        "bell\u{7}nul\u{0}\rcarriage".to_string(),
        // No whitespace to wrap at, longer than any terminal.
        "x".repeat(500),
        // Tabs, which occupy a variable number of columns.
        "a\tb\tc\td".to_string(),
        // Right-to-left text.
        "مرحبا بالعالم".to_string(),
        // Markdown the transcript highlights rather than prints.
        "```rust\nfn main() { let x: Vec<u8> = vec![]; }\n```".to_string(),
        // Inline maths, which goes through the LaTeX substitutions.
        "$\\sqrt{x^{2}} + \\mathbb{R}$".to_string(),
    ]
}

/// An `App` carrying every surface the main frame can draw at once:
/// conversation, folded and still-running tool cards, a subagent pane on
/// the rail, a todo band, the diff sidebar, and a composer with a cursor
/// somewhere in the middle of it.
fn adversarial_app() -> App {
    use crate::agent::AgentEvent;
    use crate::tools::ToolOutput;
    use crate::tools::todo::{TodoItem, TodoStatus};

    let mut app = App::new(crate::config::Config::default());
    app.welcome_dismissed = true;
    let nasty = adversarial_strings();

    for (index, text) in nasty.iter().enumerate() {
        app.handle_agent_event(AgentEvent::TextDelta(text.clone()));
        app.handle_agent_event(AgentEvent::ToolStarted {
            name: "execute".to_string(),
            args: serde_json::json!({ "command": text }),
        });
        // Alternate finished and still-running tools: a finished one takes
        // the head/tail elision path, a running one the tail-only path.
        if index % 2 == 0 {
            app.handle_agent_event(AgentEvent::ToolFinished {
                name: "execute".to_string(),
                output: ToolOutput::ok(text),
            });
        }
        app.handle_agent_event(AgentEvent::Notice(text.clone()));
    }
    app.handle_agent_event(AgentEvent::SubagentRunStarted {
        run: 1,
        bg: Some(1),
        name: nasty[3].clone(),
        task: nasty[9].clone(),
    });
    app.handle_agent_event(AgentEvent::TodoUpdated(vec![
        TodoItem {
            content: nasty[4].clone(),
            status: TodoStatus::InProgress,
        },
        TodoItem {
            content: nasty[9].clone(),
            status: TodoStatus::Pending,
        },
    ]));
    app.diff = Some(crate::app::DiffPane {
        text: nasty.join("\n"),
        scroll: 3,
    });
    app.input = nasty.join(" ");
    app.cursor = app.input.chars().count() / 2;
    app
}

/// Every skin, drawn at every shape a terminal can be, over content chosen
/// to break a width calculation.
///
/// A panic inside a renderer is not a glitched frame. It unwinds the draw,
/// runs the process-wide panic hook — which tears the terminal down — and
/// then either ends the process or leaves a live TUI painting into a
/// terminal that is no longer in raw mode. Either way what the user reports
/// is that Wizard died on its own. The widths below are the ones that
/// actually bite: one column, the widths where a two-column glyph straddles
/// the right edge, the widths where the chrome wants more columns than
/// exist, and one wider than any content.
#[test]
fn no_skin_panics_at_any_terminal_size_on_hostile_content() {
    let app = adversarial_app();
    for skin in crate::skin::Skin::ALL {
        // Thread-local, so this does not race the other render tests.
        let _pinned = crate::skin::pin(skin);
        for width in [1u16, 2, 3, 4, 5, 6, 7, 8, 12, 20, 40, 79, 80, 81, 200] {
            for height in [1u16, 2, 3, 4, 5, 8, 24, 60] {
                let backend = ratatui::backend::TestBackend::new(width, height);
                let mut terminal = ratatui::Terminal::new(backend).expect("test backend");
                terminal
                    .draw(|frame| draw(frame, &app))
                    .unwrap_or_else(|err| panic!("{width}x{height} under {skin:?}: {err}"));
            }
        }
    }
}

/// The same sweep on an untouched session, where every collection the
/// renderers index into is empty and the welcome card is what gets drawn.
#[test]
fn no_skin_panics_on_an_empty_session_at_any_terminal_size() {
    for skin in crate::skin::Skin::ALL {
        let _pinned = crate::skin::pin(skin);
        for width in [1u16, 2, 3, 5, 8, 20, 80, 200] {
            for height in [1u16, 2, 3, 5, 24, 60] {
                let app = App::new(crate::config::Config::default());
                let backend = ratatui::backend::TestBackend::new(width, height);
                let mut terminal = ratatui::Terminal::new(backend).expect("test backend");
                terminal
                    .draw(|frame| draw(frame, &app))
                    .unwrap_or_else(|err| panic!("{width}x{height} under {skin:?}: {err}"));
            }
        }
    }
}
/// The same sweep with each modal surface open in turn.
///
/// Overlays are the code most likely to build a fixed-size rect out of thin
/// air — a centred box, a one-row footer hint, a bordered card — and the
/// least likely to have been looked at on a terminal that cannot hold them.
/// They are also the worst place to crash: a plan review or an interview is
/// a turn parked on a `oneshot` waiting for an answer, so a panic there
/// loses the turn as well as the session.
#[test]
fn no_overlay_panics_at_any_terminal_size() {
    use crate::agent::{AgentEvent, ConsoleGate, InterviewGate, InterviewQuestion, PlanGate};

    let nasty = adversarial_strings();
    // Each entry rebuilds the app, because the gates below are one-shot.
    /// Opens one modal on a freshly built app.
    type OpenOverlay = fn(&mut App, &[String]);

    let overlays: Vec<(&str, OpenOverlay)> = vec![
        ("picker", |app, nasty| {
            app.picker = Some(crate::app::Picker {
                kind: crate::app::PickerKind::Mode,
                title: nasty[3].clone(),
                items: nasty
                    .iter()
                    .map(|text| crate::app::PickerItem {
                        value: text.clone(),
                        detail: text.clone(),
                        current: false,
                    })
                    .collect(),
                selected: 0,
            });
        }),
        ("plan review", |app, nasty| {
            let (gate, wait) = PlanGate::open();
            std::mem::forget(wait);
            app.handle_agent_event(AgentEvent::PlanReady {
                plan: nasty.join("\n"),
                gate,
            });
        }),
        ("interview", |app, nasty| {
            let (gate, wait) = InterviewGate::open();
            std::mem::forget(wait);
            app.handle_agent_event(AgentEvent::Interview {
                questions: nasty
                    .iter()
                    .map(|text| InterviewQuestion {
                        question: text.clone(),
                        options: nasty.to_vec(),
                    })
                    .collect(),
                gate,
            });
        }),
        ("dashboard", |app, nasty| {
            app.show_dashboard = true;
            app.sessions = vec![crate::session_registry::SessionRecord {
                id: nasty[3].clone(),
                name: nasty[4].clone(),
                cwd: nasty[9].clone(),
                model: nasty[7].clone(),
                mode: "sovereign".to_string(),
                state: crate::session_registry::SessionState::Working,
                activity: nasty[12].clone(),
                pid: 1,
                started_unix: 0,
                updated_unix: 0,
            }];
            app.peek_lines = nasty
                .iter()
                .map(|text| ("user".to_string(), text.clone()))
                .collect();
        }),
        ("console", |app, nasty| {
            let (gate, host) = ConsoleGate::open();
            std::mem::forget(host);
            app.handle_agent_event(AgentEvent::ConsoleOpened {
                command: nasty[9].clone(),
                gate,
            });
        }),
    ];

    for (label, open) in overlays {
        for skin in crate::skin::Skin::ALL {
            let _pinned = crate::skin::pin(skin);
            for width in [1u16, 2, 4, 8, 12, 20, 40, 80, 200] {
                for height in [1u16, 2, 3, 5, 10, 24, 60] {
                    let mut app = adversarial_app();
                    open(&mut app, &nasty);
                    let backend = ratatui::backend::TestBackend::new(width, height);
                    let mut terminal = ratatui::Terminal::new(backend).expect("test backend");
                    terminal
                        .draw(|frame| draw(frame, &app))
                        .unwrap_or_else(|err| {
                            panic!("{label} at {width}x{height} under {skin:?}: {err}")
                        });
                }
            }
        }
    }
}

fn card(tool: &crate::transcript::ToolItem) -> Vec<String> {
    let _skin = crate::skin::pin(crate::skin::Skin::Wizard);
    flats(&tool_card_lines(tool, false, 0, 80))
}

#[test]
fn a_finished_tool_card_carries_its_exit_code_and_elapsed_time() {
    use crate::transcript::{ToolItem, ToolItemOutput, ToolTiming};
    let tool = ToolItem {
        name: "execute".to_string(),
        args: serde_json::json!({ "command": "make" }),
        call_id: String::new(),
        output: Some(ToolItemOutput {
            content: "boom\nexit code: 2".to_string(),
            is_error: true,
        }),
        progress: String::new(),
        timing: ToolTiming {
            started: None,
            took: Some(std::time::Duration::from_millis(400)),
        },
    };
    let rows = card(&tool);
    assert_eq!(rows[0], "✕ execute  make  exit 2  0.4s", "{rows:?}");
    assert_eq!(rows[1], "╰ boom", "{rows:?}");
    assert_eq!(rows.len(), 2, "the exit line moved to the header: {rows:?}");

    // Folded, the header still says how it ended.
    let _skin = crate::skin::pin(crate::skin::Skin::Wizard);
    let folded = flats(&tool_card_lines(&tool, true, 0, 80));
    assert_eq!(folded, vec!["✕ execute  make  exit 2  0.4s  +1 lines"]);
}

#[test]
fn an_edit_card_shows_the_change_as_a_diff() {
    use crate::transcript::{ToolItem, ToolItemOutput, ToolTiming};
    let tool = ToolItem {
        name: "edit_file".to_string(),
        args: serde_json::json!({
            "path": "src/x.rs",
            "old_string": "let a = 1;",
            "new_string": "let a = 2;\nlet b = 3;",
        }),
        call_id: String::new(),
        output: Some(ToolItemOutput {
            content: "Edited src/x.rs: replaced 1 occurrence (line 12)".to_string(),
            is_error: false,
        }),
        progress: String::new(),
        timing: ToolTiming::default(),
    };
    let rows = card(&tool);
    assert_eq!(rows[0], "● edit_file  src/x.rs:12", "{rows:?}");
    assert_eq!(rows[1], "╰ - let a = 1;");
    assert_eq!(rows[2], "  + let a = 2;");
    assert_eq!(rows[3], "  + let b = 3;");
    assert!(
        !rows.iter().any(|row| row.contains("replaced")),
        "the sentence is not the body: {rows:?}"
    );
}

#[test]
fn elapsed_time_keeps_only_the_precision_it_has() {
    use std::time::Duration;
    assert_eq!(
        fmt_elapsed(Duration::from_millis(420)).as_deref(),
        Some("0.4s")
    );
    assert_eq!(fmt_elapsed(Duration::from_secs(12)).as_deref(), Some("12s"));
    assert_eq!(
        fmt_elapsed(Duration::from_secs(125)).as_deref(),
        Some("2m05s")
    );
    // A figure that would round to 0.0s is left off.
    assert_eq!(fmt_elapsed(Duration::from_millis(40)), None);
}

#[test]
fn the_busy_row_is_the_spinner_alone_unless_a_verb_was_configured() {
    let _skin = crate::skin::pin(crate::skin::Skin::Wizard);
    let mut app = App::new(crate::config::Config::default());
    app.welcome_dismissed = true;
    app.status.busy = true;
    app.turn_started = Some(std::time::Instant::now() - std::time::Duration::from_secs(3));
    let rows = render(&app);
    let screen = rows.join("\n");
    assert!(
        rows.iter().any(|row| row.trim() == "⠋ thinking"),
        "{screen}"
    );
    // One clock, on the status line.
    assert_eq!(screen.matches("3.0s").count(), 1, "{screen}");
    for verb in crate::config::UiConfig::DEFAULT_SPINNER_VERBS {
        assert!(!screen.contains(verb), "{verb} is whimsy: {screen}");
    }

    let mut config = crate::config::Config::default();
    config.ui.spinner_verbs = vec!["Pondering".to_string()];
    let mut app = App::new(config);
    app.welcome_dismissed = true;
    app.status.busy = true;
    app.turn_started = Some(std::time::Instant::now());
    let screen = render(&app).join("\n");
    assert!(screen.contains("⠋ Pondering…"), "{screen}");
    assert!(!screen.contains("Pondering… 0.0s"), "{screen}");
}

#[test]
fn the_status_line_names_the_model_mode_meter_and_branch() {
    let _skin = crate::skin::pin(crate::skin::Skin::Wizard);
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".git")).unwrap();
    std::fs::write(dir.path().join(".git/HEAD"), "ref: refs/heads/feature-x\n").unwrap();
    let mut app = App::new(crate::config::Config::default());
    app.project_root = dir.path().to_path_buf();
    let rows = render(&app);
    let status = rows.last().unwrap();
    assert!(status.contains("feature-x"), "{status}");
    assert!(
        status.contains("genie"),
        "the mode is on the line: {status}"
    );
    assert!(
        status.contains("0%"),
        "the context meter is on the line: {status}"
    );
    assert!(!status.contains("commands"), "no idle hint: {status}");
    assert!(
        !status.contains(&dir.path().display().to_string()),
        "{status}"
    );

    // Sovereign replaces the mode word.
    app.status.mode = crate::config::Mode::Sovereign;
    let rows = render(&app);
    assert!(rows.last().unwrap().contains("sovereign"));

    // A priced session shows the meter at the real percent and the cost.
    app.status.mode = crate::config::Mode::Genie;
    app.status.model = "gpt-4o".to_string();
    app.status.context_tokens = 42_000;
    app.status.prompt_tokens = 42_000;
    app.status.completion_tokens = 800;
    app.config.max_context_tokens = 128_000;
    app.config.providers.push(crate::config::ProviderConfig {
        name: "openai".to_string(),
        kind: crate::config::ProviderKind::OPENAI,
        model: "gpt-4o".to_string(),
        usd_per_mtok_in: Some(2.5),
        usd_per_mtok_out: Some(10.0),
        ..crate::config::ProviderConfig::default()
    });
    app.config.active_provider = Some("openai".to_string());
    let status = render(&app).last().unwrap().clone();
    assert!(status.contains("gpt-4o"), "{status}");
    assert!(status.contains("genie"), "{status}");
    assert!(status.contains("feature-x"), "{status}");
    assert!(status.contains("32%"), "{status}");
    assert!(status.contains("$0.11"), "{status}");

    // Outside a repository there is no branch chip.
    let bare = tempfile::tempdir().unwrap();
    app.project_root = bare.path().to_path_buf();
    let status = render(&app).last().unwrap().clone();
    assert!(!status.contains("feature-x"), "{status}");
    assert!(status.contains("gpt-4o"), "{status}");
    assert!(status.contains("32%"), "{status}");
}

#[test]
fn an_error_notice_leads_with_the_glyph() {
    let _skin = crate::skin::pin(crate::skin::Skin::Wizard);
    let mut app = App::new(crate::config::Config::default());
    app.welcome_dismissed = true;
    app.handle_agent_event(crate::agent::AgentEvent::Error(
        "provider said no".to_string(),
    ));
    let screen = render(&app).join("\n");
    assert!(screen.contains("✗ provider said no"), "{screen}");
    assert!(!screen.contains("error:"), "{screen}");
}

#[test]
fn selection_skips_the_gutter_left_of_each_rows_text_origin() {
    // An assistant block: a `· ` marker on the first row, two blank columns
    // under it, all behind a one-column margin. Counting spaces saw a shared
    // indent of one, so every row after the first pasted two spaces in.
    let mut buf = Buffer::empty(Rect::new(0, 0, 12, 3));
    buf.set_string(0, 0, " · hello", Style::default());
    buf.set_string(0, 1, "   world", Style::default());
    buf.set_string(0, 2, "     code", Style::default());
    let origins = [(0, 3), (1, 3), (2, 3)];
    assert_eq!(
        selection_text(&buf, &sel((0, 0), (11, 2)), &origins),
        "hello\nworld\n  code"
    );
    // One row dragged from the edge loses its gutter too.
    assert_eq!(
        selection_text(&buf, &sel((0, 1), (11, 1)), &origins),
        "world"
    );
}

/// Every row of one 100x60 frame, joined, for asserting on what was drawn.
fn frame_text(app: &App) -> String {
    let (width, height) = (100, 60);
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|frame| draw(frame, app)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A long tool output is never drawn open, not even for the one frame between
/// the event that made it long and the event that would have folded it.
///
/// The frame is drawn after every single event, which is what the event loop
/// does. `execute` streams its output into the card while it runs, and the
/// card used to stay open until the result landed, so a fast command drew its
/// whole output and then snapped shut.
#[test]
fn a_long_tool_output_is_folded_from_the_first_frame_it_is_in() {
    use crate::agent::{AgentEvent, ConsoleGate};
    use crate::tools::ToolOutput;

    let long: String = (1..=40).map(|n| format!("out-{n:02}\n")).collect();
    for skin in crate::skin::Skin::ALL {
        let _pinned = crate::skin::pin(skin);
        let mut app = App::new(crate::config::Config::default());
        app.transcript.set_compact(false);
        app.welcome_dismissed = true;
        let (gate, _host) = ConsoleGate::open();

        app.handle_agent_event(AgentEvent::ToolStarted {
            name: "execute".to_string(),
            args: serde_json::json!({ "command": "seq 40" }),
        });
        // Short so far: a running command's first lines stay in view.
        app.handle_agent_event(AgentEvent::ConsoleOutput {
            gate,
            chunk: "out-01\nout-02\n".to_string(),
        });
        let frame = frame_text(&app);
        assert!(
            frame.contains("out-02"),
            "{skin:?} short output shows\n{frame}"
        );

        // The chunk that makes it long folds it in the same frame.
        app.handle_agent_event(AgentEvent::ConsoleOutput {
            gate,
            chunk: long["out-01\nout-02\n".len()..].to_string(),
        });
        let frame = frame_text(&app);
        assert!(
            !frame.contains("out-"),
            "{skin:?} drew streamed output that is long\n{frame}"
        );

        app.handle_agent_event(AgentEvent::ToolFinished {
            name: "execute".to_string(),
            output: ToolOutput::ok(long.clone()),
        });
        let frame = frame_text(&app);
        assert!(
            !frame.contains("out-"),
            "{skin:?} drew the result as it landed\n{frame}"
        );

        // A tool that does not stream: folded as the result arrives.
        app.handle_agent_event(AgentEvent::ToolStarted {
            name: "read_file".to_string(),
            args: serde_json::json!({ "path": "big.txt" }),
        });
        app.handle_agent_event(AgentEvent::ToolFinished {
            name: "read_file".to_string(),
            output: ToolOutput::ok(long.replace("out-", "read-")),
        });
        let frame = frame_text(&app);
        assert!(
            !frame.contains("read-"),
            "{skin:?} drew a long read as it landed\n{frame}"
        );
    }
}

/// A turn with everything compact view hides and everything it keeps.
fn a_busy_turn() -> App {
    use crate::agent::AgentEvent;
    use crate::tools::ToolOutput;

    let mut app = App::new(crate::config::Config::default());
    app.welcome_dismissed = true;
    app.transcript
        .user("fix the parser".to_string(), Vec::new());
    app.handle_agent_event(AgentEvent::ThinkingDelta("mulling-it-over".to_string()));
    app.handle_agent_event(AgentEvent::TextDelta("Looking at it now.".to_string()));
    app.handle_agent_event(AgentEvent::ToolStarted {
        name: "read_file".to_string(),
        args: serde_json::json!({ "path": "parser.rs" }),
    });
    app.handle_agent_event(AgentEvent::ToolFinished {
        name: "read_file".to_string(),
        output: ToolOutput::ok("fn parse-body() {}"),
    });
    app.handle_agent_event(AgentEvent::ToolStarted {
        name: "edit_file".to_string(),
        args: serde_json::json!({
            "path": "parser.rs",
            "old_string": "old-diff-line",
            "new_string": "new-diff-line",
        }),
    });
    app.handle_agent_event(AgentEvent::ToolFinished {
        name: "edit_file".to_string(),
        output: ToolOutput::ok("Edited parser.rs"),
    });
    app.handle_agent_event(AgentEvent::ToolStarted {
        name: "execute".to_string(),
        args: serde_json::json!({ "command": "cargo test" }),
    });
    app.handle_agent_event(AgentEvent::ToolFinished {
        name: "execute".to_string(),
        output: ToolOutput::error("test-output-failed\nexit code: 101"),
    });
    app.handle_agent_event(AgentEvent::TextDelta("Fixed it.".to_string()));
    app.handle_agent_event(AgentEvent::Error("provider-said-no".to_string()));
    app
}

#[test]
fn compact_view_keeps_the_conversation_and_drops_the_work() {
    for skin in crate::skin::Skin::ALL {
        let _pinned = crate::skin::pin(skin);
        let mut app = a_busy_turn();
        app.transcript.set_compact(false);

        let full = frame_text(&app);
        for shown in ["fix the parser", "Looking at it now.", "test-output-failed"] {
            assert!(
                full.contains(shown),
                "{skin:?} full view lost {shown}\n{full}"
            );
        }

        app.transcript.set_compact(true);
        let compact = frame_text(&app);
        for kept in [
            "fix the parser",
            "Looking at it now.",
            "Fixed it.",
            "provider-said-no",
            "▸ ran 1 command · edited 1 file · read 1 file · 1 failed",
        ] {
            assert!(
                compact.contains(kept),
                "{skin:?} compact lost {kept}\n{compact}"
            );
        }
        for hidden in [
            "parse-body",
            "diff-line",
            "test-output-failed",
            "cargo test",
            "parser.rs",
            "mulling-it-over",
        ] {
            assert!(
                !compact.contains(hidden),
                "{skin:?} compact drew {hidden}\n{compact}"
            );
        }
    }
}

#[test]
fn compact_view_names_the_tool_that_is_running() {
    use crate::agent::AgentEvent;
    for skin in crate::skin::Skin::ALL {
        let _pinned = crate::skin::pin(skin);
        let mut app = a_busy_turn();
        app.transcript.set_compact(true);
        app.handle_agent_event(AgentEvent::ToolStarted {
            name: "execute".to_string(),
            args: serde_json::json!({ "command": "make" }),
        });
        let frame = frame_text(&app);
        assert!(frame.contains("▸ running make"), "{skin:?}\n{frame}");
    }
}

#[test]
fn view_toggles_the_transcript_that_is_already_there() {
    let _pinned = crate::skin::pin(crate::skin::Skin::Wizard);
    let mut app = a_busy_turn();
    assert!(app.transcript.compact(), "on by default");
    assert!(!frame_text(&app).contains("parse-body"));

    app.set_compact_view(None);
    assert!(!app.transcript.compact() && !app.config.ui.compact);
    assert!(frame_text(&app).contains("parse-body"));

    app.set_compact_view(Some(false));
    assert!(
        !app.transcript.compact(),
        "naming the view it is in keeps it"
    );

    app.set_compact_view(Some(true));
    assert!(app.config.ui.compact);
    assert!(!frame_text(&app).contains("parse-body"));
}

#[test]
fn compact_in_the_config_starts_the_session_compact() {
    let mut config = crate::config::Config::default();
    config.ui.compact = true;
    assert!(App::new(config).transcript.compact());
}

fn pin_house() -> (crate::skin::Pinned, theme::Pinned) {
    (
        crate::skin::pin(crate::skin::Skin::Wizard),
        theme::pin(std::sync::Arc::new(
            theme::load("wizard")
                .expect("wizard theme")
                .with_depth(theme::ColorDepth::TrueColor),
        )),
    )
}

fn screen_at(app: &App, width: u16, height: u16) -> (Vec<String>, ratatui::buffer::Buffer) {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|frame| draw(frame, app)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    let rows = (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect();
    (rows, buffer)
}

fn buffer_colors(buffer: &ratatui::buffer::Buffer) -> Vec<Color> {
    let area = buffer.area;
    let mut colors = Vec::new();
    for y in 0..area.height {
        for x in 0..area.width {
            let cell = &buffer[(x, y)];
            colors.push(cell.fg);
            colors.push(cell.bg);
        }
    }
    colors
}

/// Mid-stream: the partial reply is on screen with the streaming caret, and
/// the composer is still the composer.
#[test]
fn a_streaming_reply_keeps_its_caret_and_the_composer() {
    let _pins = pin_house();
    let mut app = App::new(crate::config::Config::default());
    app.welcome_dismissed = true;
    app.transcript
        .user("explain the parser".to_string(), Vec::new());
    app.handle_agent_event(crate::agent::AgentEvent::TextDelta(
        "The parser walks each token".to_string(),
    ));
    app.status.busy = true;
    app.tick = 0;
    let (rows, _) = screen_at(&app, 80, 24);
    let screen = rows.join("\n");
    assert!(screen.contains("The parser walks each token"), "{screen}");
    assert!(screen.contains('▍'), "streaming caret:\n{screen}");
    assert!(
        rows.iter().any(|row| row.contains('❯')),
        "composer:\n{screen}"
    );
}

/// A tool call is one header: glyph, name, argument. Not a panel.
#[test]
fn a_tool_call_is_a_single_header_row() {
    let _pins = pin_house();
    let mut app = App::new(crate::config::Config::default());
    app.transcript.set_compact(false);
    app.welcome_dismissed = true;
    app.transcript.user("run the tests".to_string(), Vec::new());
    app.handle_agent_event(crate::agent::AgentEvent::ToolStarted {
        name: "execute".to_string(),
        args: serde_json::json!({ "command": "cargo test" }),
    });
    app.tick = 0;
    let (rows, _) = screen_at(&app, 80, 24);
    let header = rows
        .iter()
        .find(|row| row.contains("execute") && row.contains("cargo test"))
        .expect("tool header");
    assert!(
        header.contains('⠋'),
        "a running call shows the spinner, not a box: {header}"
    );
    assert!(!header.contains('┌') && !header.contains('│'), "{header}");
}

/// Every command the TUI can run shows up in the slash menu, by name.
#[test]
fn the_slash_menu_offers_every_tui_command() {
    let _pins = pin_house();
    let available = crate::commands::available(crate::commands::Surface::Tui);
    for spec in crate::commands::COMMANDS {
        let listed = available.iter().any(|row| row.name == spec.name);
        let runnable = spec.tui != crate::commands::Execution::Unavailable;
        assert_eq!(listed, runnable, "/{}", spec.name);
    }
    let mut app = App::new(crate::config::Config::default());
    app.welcome_dismissed = true;
    app.input = "/".to_string();
    app.suggestions = available.iter().map(crate::app::Suggestion::from).collect();
    app.input_mode = crate::app::InputMode::Command;
    for index in 0..app.suggestions.len() {
        app.suggestion_index = index;
        let name = app.suggestions[index].name.clone();
        let (rows, _) = screen_at(&app, 80, 24);
        let screen = rows.join("\n");
        assert!(
            screen.contains(&format!("/{name}")) || screen.contains(&name),
            "/{name} missing from the menu:\n{screen}"
        );
        assert!(screen.contains('❯'), "selection marker:\n{screen}");
    }
}

/// Several runs are one summary until the rail is focused, then a list, and
/// opening one shows its own transcript with a way back and a follow state.
#[test]
fn subagents_summarize_then_open_without_a_pane_grid() {
    let _pins = pin_house();
    let mut app = App::new(crate::config::Config::default());
    app.welcome_dismissed = true;
    app.transcript
        .user("delegate the review".to_string(), Vec::new());
    for (run, name) in [(1u64, "researcher"), (2, "reviewer"), (3, "tester")] {
        app.handle_agent_event(crate::agent::AgentEvent::SubagentRunStarted {
            run,
            bg: Some(run as u32),
            name: name.to_string(),
            task: format!("task for {name}"),
        });
    }
    app.handle_agent_event(crate::agent::AgentEvent::SubagentRunToolStarted {
        run: 1,
        name: "read_file".to_string(),
        args: serde_json::json!({}),
    });
    app.tick = 0;

    let (rows, _) = screen_at(&app, 80, 24);
    let screen = rows.join("\n");
    let summary = rows
        .iter()
        .find(|row| row.contains("running"))
        .expect("summary row");
    assert!(summary.contains("researcher"), "{summary}");
    assert!(summary.contains("reviewer"), "{summary}");
    assert!(
        !screen.contains("task for reviewer"),
        "the task list is not a grid:\n{screen}"
    );
    assert_eq!(rail_height(&app), 1);

    assert!(app.focus_rail());
    let (focused, _) = screen_at(&app, 80, 24);
    let joined = focused.join("\n");
    assert!(joined.contains('❯'), "focused rail has a cursor:\n{joined}");
    assert!(joined.contains("researcher"), "{joined}");
    assert!(joined.contains("tester"), "{joined}");
    assert!(rail_height(&app) > 1);

    app.attach_pane(0);
    app.handle_agent_event(crate::agent::AgentEvent::SubagentRunText {
        run: 1,
        text: "auth starts in login.rs".to_string(),
    });
    let (inside, _) = screen_at(&app, 80, 24);
    let inside_text = inside.join("\n");
    assert!(inside_text.contains("researcher"), "{inside_text}");
    assert!(inside_text.contains("running"), "{inside_text}");
    assert!(
        inside_text.contains("auth starts in login.rs"),
        "{inside_text}"
    );
    assert!(inside_text.contains("esc back"), "{inside_text}");
    assert!(
        !inside_text.contains("delegate the review"),
        "the main chat yields:\n{inside_text}"
    );
    assert_eq!(rail_height(&app), 1, "siblings stay a summary");

    app.panes[0].transcript.follow = false;
    let (scrolled, _) = screen_at(&app, 80, 24);
    let scrolled_text = scrolled.join("\n");
    assert!(scrolled_text.contains("scrolled"), "{scrolled_text}");
    assert!(scrolled_text.contains("ctrl-end follow"), "{scrolled_text}");
}

/// 40 columns: the summary, the tool header, and the menu still read.
#[test]
fn a_narrow_terminal_keeps_the_summary_and_drops_nothing_essential() {
    let _pins = pin_house();
    let mut app = App::new(crate::config::Config::default());
    app.welcome_dismissed = true;
    app.transcript.user("narrow".to_string(), Vec::new());
    app.handle_agent_event(crate::agent::AgentEvent::TextDelta(
        "still streaming on a narrow screen".to_string(),
    ));
    app.handle_agent_event(crate::agent::AgentEvent::ToolStarted {
        name: "execute".to_string(),
        args: serde_json::json!({ "command": "cargo test --all" }),
    });
    for (run, name) in [(1u64, "researcher"), (2, "reviewer")] {
        app.handle_agent_event(crate::agent::AgentEvent::SubagentRunStarted {
            run,
            bg: Some(run as u32),
            name: name.to_string(),
            task: "a long task description that must not wrap the header".to_string(),
        });
    }
    let (rows, buffer) = screen_at(&app, 40, 20);
    let screen = rows.join("\n");
    assert!(screen.contains("running"), "{screen}");
    assert!(screen.contains("researcher"), "{screen}");
    assert!(
        screen.contains('▍') || screen.contains("execute") || screen.contains('▸'),
        "{screen}"
    );
    // Every painted row fits. A cell past the edge would have panicked;
    // this checks the summary itself was cut with an ellipsis rather than
    // run on into the next row.
    let summary = rows
        .iter()
        .find(|row| row.contains("running"))
        .expect("summary");
    assert!(summary.width() <= 40, "{summary}");
    assert_eq!(buffer.area.width, 40);

    app.input = "/".to_string();
    app.suggestions = crate::commands::available(crate::commands::Surface::Tui)
        .iter()
        .take(4)
        .map(crate::app::Suggestion::from)
        .collect();
    app.suggestion_index = 0;
    app.input_mode = crate::app::InputMode::Command;
    let (menu, _) = screen_at(&app, 40, 20);
    let menu_text = menu.join("\n");
    assert!(
        menu_text.contains("/model") || menu_text.contains("model"),
        "{menu_text}"
    );
    assert!(menu.iter().all(|row| row.width() <= 40), "{menu_text}");
}

/// Without truecolor the frame uses the degraded palette: no 24-bit cells.
#[test]
fn the_frame_degrades_without_truecolor() {
    let _skin = crate::skin::pin(crate::skin::Skin::Wizard);
    let mut app = App::new(crate::config::Config::default());
    app.welcome_dismissed = true;
    app.transcript.user("color".to_string(), Vec::new());
    app.handle_agent_event(crate::agent::AgentEvent::TextDelta(
        "a short reply".to_string(),
    ));
    app.handle_agent_event(crate::agent::AgentEvent::ToolStarted {
        name: "execute".to_string(),
        args: serde_json::json!({ "command": "true" }),
    });

    for depth in [theme::ColorDepth::Ansi16, theme::ColorDepth::Mono] {
        let _pin = theme::pin(std::sync::Arc::new(theme::minimal().with_depth(depth)));
        let (_, buffer) = screen_at(&app, 80, 24);
        for color in buffer_colors(&buffer) {
            assert!(
                !matches!(color, Color::Rgb(..)),
                "{depth:?} painted truecolor {color:?}"
            );
            if depth == theme::ColorDepth::Mono {
                assert!(
                    matches!(color, Color::Reset),
                    "mono should be reset, got {color:?}"
                );
            }
        }
    }
}

fn work_turn() -> App {
    let mut app = a_busy_turn();
    app.transcript.set_compact(true);
    app
}

fn click_row(app: &mut App, row: u16) {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        app.handle_event(crate::event::Event::Mouse(MouseEvent {
            kind,
            column: 4,
            row,
            modifiers: KeyModifiers::NONE,
        }))
        .unwrap();
    }
}

/// Compact draws the conversation and one summary. Verbose draws the work.
#[test]
fn compact_and_verbose_draw_different_frames() {
    let _pins = pin_house();
    let mut app = work_turn();
    let (compact, _) = screen_at(&app, 80, 24);
    let compact_text = compact.join("\n");
    assert!(
        compact_text.contains("▸ ran 1 command · edited 1 file · read 1 file · 1 failed"),
        "{compact_text}"
    );
    assert!(
        compact_text.contains("Looking at it now."),
        "{compact_text}"
    );
    assert!(compact_text.contains("fix the parser"), "{compact_text}");
    assert!(!compact_text.contains("parse-body"), "{compact_text}");
    assert!(!compact_text.contains("old-diff-line"), "{compact_text}");

    app.transcript.set_compact(false);
    let (verbose, _) = screen_at(&app, 80, 24);
    let verbose_text = verbose.join("\n");
    assert!(verbose_text.contains("parse-body"), "{verbose_text}");
    assert!(verbose_text.contains("execute"), "{verbose_text}");
    assert!(!verbose_text.contains('▸'), "{verbose_text}");
}

#[test]
fn a_click_opens_the_summary_and_a_second_click_shuts_it() {
    let _pins = pin_house();
    let mut app = work_turn();
    let _ = screen_at(&app, 80, 24);
    let row = app
        .group_hits
        .borrow()
        .first()
        .map(|(row, _)| *row)
        .expect("the summary is clickable");
    click_row(&mut app, row);
    let (open, _) = screen_at(&app, 80, 24);
    let open_text = open.join("\n");
    assert!(open_text.contains('▾'), "{open_text}");
    assert!(open_text.contains("parse-body"), "{open_text}");
    assert!(open_text.contains("- old-diff-line"), "{open_text}");
    assert!(open_text.contains("+ new-diff-line"), "{open_text}");

    let row = app
        .group_hits
        .borrow()
        .first()
        .map(|(row, _)| *row)
        .expect("the open summary is still clickable");
    click_row(&mut app, row);
    let (shut, _) = screen_at(&app, 80, 24);
    let shut_text = shut.join("\n");
    assert!(shut_text.contains('▸'), "{shut_text}");
    assert!(!shut_text.contains("parse-body"), "{shut_text}");
}

#[test]
fn tab_and_enter_open_the_focused_summary() {
    let _pins = pin_house();
    let mut app = work_turn();
    let action = app
        .handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Tab,
            crossterm::event::KeyModifiers::NONE,
        ))
        .unwrap();
    assert!(action.is_none());
    assert!(app.transcript.focused_group().is_some());
    let (focused, _) = screen_at(&app, 80, 24);
    assert!(focused.join("\n").contains('▸'));

    app.handle_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::NONE,
    ))
    .unwrap();
    let (open, _) = screen_at(&app, 80, 24);
    let open_text = open.join("\n");
    assert!(open_text.contains('▾'), "{open_text}");
    assert!(open_text.contains("cargo test"), "{open_text}");
}

#[test]
fn ctrl_t_opens_every_summary_and_ctrl_o_flips_the_saved_view() {
    let _pins = pin_house();
    let mut app = work_turn();
    app.handle_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('t'),
        crossterm::event::KeyModifiers::CONTROL,
    ))
    .unwrap();
    let opened = screen_at(&app, 80, 24).0.join("\n");
    assert!(opened.contains("parse-body"), "{opened}");

    app.handle_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('o'),
        crossterm::event::KeyModifiers::CONTROL,
    ))
    .unwrap();
    assert!(!app.transcript.compact());
    assert!(!app.config.ui.compact);
    let verbose = screen_at(&app, 80, 24).0.join("\n");
    assert!(verbose.contains("execute"), "{verbose}");

    let saved = toml::to_string(&app.config).unwrap();
    let loaded: crate::config::Config = toml::from_str(&saved).unwrap();
    assert!(!loaded.ui.compact, "the flipped view survives a save");
}

#[test]
fn markdown_renders_a_heading_a_list_and_inline_code() {
    let _pins = pin_house();
    let rendered = render_markdown_at("# Title\n\n- one item\n\nuse `parse` here\n", 60);
    let text = rendered
        .lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("Title"), "{text}");
    assert!(text.contains("one item"), "{text}");
    assert!(text.contains("parse"), "{text}");
    let code = rendered
        .lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.contains("parse"));
    let code = code.expect("inline code");
    assert!(
        code.style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD)
    );
}

#[test]
fn a_summary_fits_a_narrow_terminal_and_a_wider_one() {
    let _pins = pin_house();
    let app = work_turn();
    for width in [40u16, 100] {
        let (rows, buffer) = screen_at(&app, width, 20);
        let screen = rows.join("\n");
        assert!(screen.contains("▸"), "{width}: {screen}");
        assert!(screen.contains("command"), "{width}: {screen}");
        assert!(
            rows.iter().all(|row| row.width() <= width as usize),
            "{width}: {screen}"
        );
        assert_eq!(buffer.area.width, width);
    }
}

/// A resize is a new frame of the same conversation: the summary survives,
/// and the buffer's size is the size it was given.
#[test]
fn a_resize_redraws_the_compact_frame_at_the_new_size() {
    let _pins = pin_house();
    let app = work_turn();
    let (wide, wide_buf) = screen_at(&app, 100, 30);
    let (narrow, narrow_buf) = screen_at(&app, 44, 16);
    assert_eq!((wide_buf.area.width, wide_buf.area.height), (100, 30));
    assert_eq!((narrow_buf.area.width, narrow_buf.area.height), (44, 16));
    assert_ne!(wide, narrow, "the resize painted the same frame");
    for (label, rows, width) in [("wide", &wide, 100usize), ("narrow", &narrow, 44)] {
        let screen = rows.join("\n");
        assert!(screen.contains('▸'), "{label}: {screen}");
        assert!(screen.contains("command"), "{label}: {screen}");
        assert!(
            rows.iter().all(|row| row.width() <= width),
            "{label} overflowed {width}: {screen}"
        );
    }
}

/// White ink disappears on a light terminal. The house palette does not use it.
#[test]
fn the_house_palette_reads_on_a_light_terminal() {
    let theme = theme::load("minimal").expect("minimal");
    for token in theme::Token::ALL {
        assert_ne!(
            theme.declared(token),
            Color::White,
            "{} is white",
            token.key()
        );
    }
    let _pin = theme::pin(std::sync::Arc::new(theme));
    let (_, buffer) = screen_at(&work_turn(), 80, 24);
    for color in buffer_colors(&buffer) {
        assert_ne!(color, Color::White, "a cell painted white");
    }
}

/// On a dark terminal the same palette is grey chrome and the terminal's own
/// foreground, with no filled background.
#[test]
fn the_house_palette_reads_on_a_dark_terminal() {
    let _pins = pin_house();
    let (_, buffer) = screen_at(&work_turn(), 80, 24);
    let colors = buffer_colors(&buffer);
    assert!(
        colors.contains(&Color::Reset),
        "body ink follows the terminal"
    );
    assert!(
        colors.contains(&Color::DarkGray),
        "chrome is grey on a dark ground"
    );
    assert!(!colors.contains(&Color::White));
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            assert_eq!(buffer[(x, y)].bg, Color::Reset);
        }
    }
}

/// The house palette names an accent and still paints no background, so a
/// light or transparent terminal shows through every cell.
#[test]
fn the_wizard_theme_paints_no_background() {
    let theme = theme::load("wizard").expect("wizard theme");
    for token in [
        theme::Token::BgBase,
        theme::Token::BgRaised,
        theme::Token::BgSunken,
    ] {
        assert_eq!(theme.declared(token), Color::Reset, "{}", token.key());
    }
    let _skin = crate::skin::pin(crate::skin::Skin::Wizard);
    let _theme = theme::pin(std::sync::Arc::new(theme));
    let (_, buffer) = screen_at(&work_turn(), 100, 28);
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            assert_eq!(buffer[(x, y)].bg, Color::Reset, "filled cell at {x},{y}");
        }
    }
}

/// A wide frame with a running agent uses the side column. A narrow one keeps
/// the single summary row and does not open that column.
#[test]
fn the_side_rail_opens_at_120_columns_and_stays_shut_below() {
    let _pins = pin_house();
    let mut app = App::new(crate::config::Config::default());
    app.handle_agent_event(crate::agent::AgentEvent::SubagentRunStarted {
        run: 1,
        bg: None,
        name: "researcher".to_string(),
        task: "read the parser".to_string(),
    });
    let wide = regions(&app, ratatui::layout::Rect::new(0, 0, 132, 36));
    assert!(wide.side.width >= 30, "side column: {:?}", wide.side);
    assert_eq!(wide.rail.height, 0, "the bottom rail yields to the column");
    let narrow = regions(&app, ratatui::layout::Rect::new(0, 0, 80, 24));
    assert_eq!(narrow.side.width, 0);
    assert_eq!(narrow.rail.height, 1);

    let (rows, buffer) = screen_at(&app, 132, 36);
    let screen = rows.join("\n");
    assert!(
        screen.contains('│') || screen.contains("researcher"),
        "{screen}"
    );
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            assert_eq!(buffer[(x, y)].bg, Color::Reset);
        }
    }
}

/// Foreground color is what a truecolor terminal gets, and `NO_COLOR` takes
/// it away. Backgrounds stay unset either way.
///
/// The cream and slate screenshots that came back unpainted were the first
/// case: this environment exports `NO_COLOR=1`, and that forces
/// [`ColorDepth::Mono`](theme::ColorDepth::Mono) no matter what `COLORTERM`
/// says. The cells below are what the same frame emits once that is unset.
#[test]
fn colors_are_emitted_in_truecolor_and_suppressed_under_no_color() {
    use crate::agent::AgentEvent;
    use crate::tools::ToolOutput;

    assert_eq!(
        theme::ColorDepth::from_env(None, Some("1"), Some("truecolor"), Some("xterm-256color")),
        theme::ColorDepth::Mono,
        "a non-empty NO_COLOR is monochrome even in a truecolor terminal"
    );
    assert_eq!(
        theme::ColorDepth::from_env(None, None, Some("truecolor"), Some("xterm-256color")),
        theme::ColorDepth::TrueColor
    );

    let wizard = theme::load("wizard").expect("wizard theme");
    let accent = wizard.declared(theme::Token::Accent);
    assert!(
        matches!(accent, Color::Rgb(_, _, _)),
        "the house accent is a real color, got {accent:?}"
    );
    let _skin = crate::skin::pin(crate::skin::Skin::Wizard);

    let mut app = App::new(crate::config::Config::default());
    app.welcome_dismissed = true;
    app.transcript.set_compact(false);
    app.transcript
        .user("count the images".to_string(), Vec::new());
    app.handle_agent_event(AgentEvent::ToolStarted {
        name: "edit_file".to_string(),
        args: serde_json::json!({
            "path": "src/transcript/fold.rs",
            "old_string": "tools += 1;",
            "new_string": "tools += 1;\nimages += t.output.images().count();",
        }),
    });
    app.handle_agent_event(AgentEvent::ToolFinished {
        name: "edit_file".to_string(),
        output: ToolOutput::ok("Edited src/transcript/fold.rs (line 12)"),
    });

    let _pin = theme::pin(std::sync::Arc::new(
        wizard.clone().with_depth(theme::ColorDepth::TrueColor),
    ));
    let (_, buffer) = screen_at(&app, 100, 28);
    let mut saw_accent = false;
    let mut saw_add = false;
    let mut saw_del = false;
    let mut saw_dim = false;
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            let cell = &buffer[(x, y)];
            assert_eq!(cell.bg, Color::Reset, "filled cell at {x},{y}");
            if (cell.symbol() == "◆" || cell.symbol() == "❯") && cell.fg == accent {
                saw_accent = true;
            }
            if cell.fg == Color::Green {
                saw_add = true;
            }
            if cell.fg == Color::Red {
                saw_del = true;
            }
            if cell.fg == Color::DarkGray {
                saw_dim = true;
            }
        }
    }
    assert!(saw_accent, "accent foreground was not emitted");
    assert!(saw_add, "diff additions were not green");
    assert!(saw_del, "diff deletions were not red");
    assert!(saw_dim, "dim chrome was not darker than the body");

    let _pin = theme::pin(std::sync::Arc::new(
        wizard.with_depth(theme::ColorDepth::Mono),
    ));
    let (_, buffer) = screen_at(&app, 100, 28);
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            let cell = &buffer[(x, y)];
            assert_eq!(
                cell.fg,
                Color::Reset,
                "mono still painted {}",
                cell.symbol()
            );
            assert_eq!(cell.bg, Color::Reset);
        }
    }
}

/// A long activity line wraps inside the rail. The continuation keeps the
/// rule and the indent; it does not start on the rule's column.
#[test]
fn rail_text_wraps_inside_the_column() {
    use crate::agent::AgentEvent;
    let _pins = pin_house();
    let mut app = App::new(crate::config::Config::default());
    app.welcome_dismissed = true;
    app.handle_agent_event(AgentEvent::SubagentRunStarted {
        run: 1,
        bg: Some(1),
        name: "researcher".to_string(),
        task: "where is the parser".to_string(),
    });
    let activity = "The parser lives in src/transcript/fold.rs and the summary skips every image.";
    app.handle_agent_event(AgentEvent::SubagentRunText {
        run: 1,
        text: activity.to_string(),
    });
    app.handle_agent_event(AgentEvent::SubagentRunStep { run: 1, step: 4 });

    let (rows, buffer) = screen_at(&app, 132, 36);
    let side = regions(&app, ratatui::layout::Rect::new(0, 0, 132, 36)).side;
    assert!(side.width >= 30);
    let mut rail_rows = Vec::new();
    for y in side.y..side.y + side.height {
        let row: String = (side.x..side.x + side.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect();
        let trimmed = row.trim_end().to_string();
        if trimmed.is_empty() {
            continue;
        }
        assert!(
            trimmed.starts_with('│'),
            "rail row left the rule: {trimmed:?}\n{}",
            rows.join("\n")
        );
        rail_rows.push(trimmed);
    }
    let joined = rail_rows.join("\n");
    assert!(
        joined.contains("fold.rs"),
        "the activity was cut off entirely: {joined}"
    );
    assert!(
        joined.contains("4/50"),
        "steps should read n/budget, got {joined}"
    );
    assert!(
        !rail_rows
            .iter()
            .any(|row| row.trim() == "│ 0" || row.ends_with(" 0")),
        "a bare step count: {joined}"
    );
    // The wrapped tail is on its own row and still behind the rule.
    assert!(
        rail_rows
            .iter()
            .any(|row| row.contains("fold.rs") && row.starts_with("│ ")),
        "{joined}"
    );
}

/// Expanded background and subagent results are one human line, not the
/// sentence the model was given.
#[test]
fn harness_boilerplate_renders_as_one_human_line() {
    use crate::agent::AgentEvent;
    use crate::tools::ToolOutput;
    let _pins = pin_house();
    let mut app = App::new(crate::config::Config::default());
    app.welcome_dismissed = true;
    app.transcript.set_compact(false);
    app.transcript
        .user("find the parser".to_string(), Vec::new());
    app.handle_agent_event(AgentEvent::ToolStarted {
        name: "execute".to_string(),
        args: serde_json::json!({
            "command": "printf 'hello'",
            "run_in_background": true,
        }),
    });
    app.handle_agent_event(AgentEvent::ToolFinished {
        name: "execute".to_string(),
        output: ToolOutput::ok(
            "Background task #1 started: printf 'hello'\nYou will be notified when it finishes; \
             use task_output to inspect it or task_kill to stop it.",
        ),
    });
    app.handle_agent_event(AgentEvent::ToolStarted {
        name: "spawn_subagent".to_string(),
        args: serde_json::json!({ "subagent": "researcher", "task": "where is the parser" }),
    });
    app.handle_agent_event(AgentEvent::ToolFinished {
        name: "spawn_subagent".to_string(),
        output: ToolOutput::ok(
            "Delegated to subagent 'researcher' (#1): where is the parser.\nRunning in the \
             background — you'll see its progress as it works, and the report lands in your \
             context once it's done.",
        ),
    });
    let screen = screen_at(&app, 100, 32).0.join("\n");
    assert!(
        screen.contains("started in background · ctrl+b output"),
        "{screen}"
    );
    assert!(screen.contains("researcher · running"), "{screen}");
    assert!(!screen.contains("You will be notified"), "{screen}");
    assert!(!screen.contains("you'll see its progress"), "{screen}");
}

/// A small word change is bold and underlined, and the row itself is not filled.
#[test]
fn a_diff_marks_the_changed_word_without_a_fill() {
    use crate::transcript::{ToolItem, ToolItemOutput, ToolTiming};
    use ratatui::style::Modifier;
    let _skin = crate::skin::pin(crate::skin::Skin::Wizard);
    let tool = ToolItem {
        name: "edit_file".to_string(),
        args: serde_json::json!({
            "path": "src/x.rs",
            "old_string": "let a = 1;",
            "new_string": "let a = 2;",
        }),
        call_id: String::new(),
        output: Some(ToolItemOutput {
            content: "Edited src/x.rs (line 1)".to_string(),
            is_error: false,
        }),
        progress: String::new(),
        timing: ToolTiming::default(),
    };
    let lines = tool_card_lines(&tool, false, 0, 80);
    let marked = lines.iter().any(|line| {
        line.spans.iter().any(|span| {
            span.style
                .add_modifier
                .contains(Modifier::BOLD | Modifier::UNDERLINED)
                && matches!(span.style.bg, Some(Color::Reset) | None)
        })
    });
    assert!(marked, "the changed digit should be underlined: {lines:?}");
}
