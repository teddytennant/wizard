//! Codex's own markdown render snapshots (`tui/src/snapshots/*markdown_render_tests*`), run
//! through the ported renderer. The sources and expected rows are copied under
//! `tests/fixtures/md`.

use std::path::{Path, PathBuf};

use codexw::markdown::agent::render_markdown_agent;

fn fixture(name: &str) -> String {
    std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/md")
            .join(name),
    )
    .unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn check(name: &str, width: Option<usize>, cwd: Option<&str>) {
    let md = fixture(&format!("{name}.md"));
    let want = fixture(&format!("{name}.snap"));
    let lines = render_markdown_agent(&md, width, cwd.map(Path::new));
    let got = lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(got.trim_end(), want.trim_end(), "{name} at {width:?}");
}

#[test]
fn multiline_finding_items_are_separated() {
    check("multiline_finding_items_are_separated_snapshot", None, None);
}

#[test]
fn table_wraps_file_paths_before_collapsing_narrative_columns() {
    check(
        "table_wraps_file_paths_before_collapsing_narrative_columns_snapshot",
        Some(120),
        Some("/Users/example/code/codex"),
    );
}

#[test]
fn stacked_key_value_records_when_the_path_column_gets_too_narrow() {
    check(
        "table_renders_stacked_key_value_records_when_path_column_becomes_too_narrow_snapshot",
        Some(42),
        None,
    );
}

#[test]
fn records_when_several_prose_columns_are_starved() {
    check(
        "table_renders_records_when_multiple_prose_columns_are_starved_snapshot",
        Some(76),
        None,
    );
}

#[test]
fn grid_stays_when_only_one_compact_record_fragments() {
    check(
        "table_keeps_grid_when_only_one_compact_record_fragments_snapshot",
        Some(40),
        None,
    );
}

#[test]
fn records_when_compact_fragmentation_is_systemic() {
    check(
        "table_renders_key_value_records_when_compact_fragmentation_is_systemic_snapshot",
        Some(17),
        None,
    );
}

#[test]
fn halfwidth_sound_marks_count_as_one_cell() {
    // The snapshot holds the grid at 23 columns, then the records layout at 17.
    let name = "table_renders_halfwidth_sound_marks_at_constrained_width_snapshot";
    let md = fixture(&format!("{name}.md"));
    let snap = fixture(&format!("{name}.snap"));
    let (grid, records) = snap
        .split_once("\n\nrecords (17 cells):\n")
        .expect("two blocks");
    let grid = grid.strip_prefix("grid (23 cells):\n").expect("grid block");
    for (want, width) in [(grid, 23usize), (records, 17)] {
        let got = render_markdown_agent(&md, Some(width), None)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(got.trim_end(), want.trim_end(), "width {width}");
    }
}

#[test]
fn complex_document() {
    check("markdown_render_complex_snapshot", None, None);
}

#[test]
fn prose_around_a_link_wraps_by_word() {
    check("mixed_url", Some(48), None);
}

#[test]
fn local_file_link_shows_its_target_relative_to_the_cwd() {
    check("file_link", None, Some("/Users/example/code/codex"));
}
