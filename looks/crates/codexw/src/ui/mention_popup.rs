// OWNER: bottom-pane (slash popup, @ mentions)
//! The `@` mention popup (spec C.3.3), ported from Codex's `mentions_v2` popup. Wizard has no
//! plugins, so the rows are its skills (listed first, and alone for an empty query, as Codex
//! does) and the project's files and directories (from `git ls-files`, or a directory walk
//! outside a repository); the search-mode footer is kept as drawn. With no skills at all an
//! empty query lists files, so the popup is never blank for a user who has none.

use std::path::Path;
use std::sync::Arc;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use super::footer::FOOTER_INDENT_COLS;
use super::slash_popup::{MAX_POPUP_ROWS, ScrollState};
use super::status_indicator::truncate_with_ellipsis;
use crate::skills::Skill;
use crate::style::palette;
use crate::wrap::{line_width, width_of};

const TAG_WIDTH: usize = 6;
const MAX_FILES: usize = 100_000;

/// Every file and directory under the working directory, relative paths.
#[derive(Debug, Default)]
pub struct FileIndex {
    pub files: Vec<String>,
    /// Directories with a trailing `/`.
    pub dirs: Vec<String>,
}

impl FileIndex {
    pub fn from_files(mut files: Vec<String>) -> Self {
        files.sort();
        files.truncate(MAX_FILES);
        let mut dirs: Vec<String> = Vec::new();
        for f in &files {
            let mut end = 0;
            while let Some(i) = f[end..].find('/') {
                end += i + 1;
                let d = &f[..end];
                if dirs.last().map(String::as_str) != Some(d) && !dirs.iter().any(|x| x == d) {
                    dirs.push(d.to_string());
                }
            }
        }
        dirs.sort();
        FileIndex { files, dirs }
    }

    /// Git's view of the tree when `cwd` is inside a repository, else a bounded directory walk.
    pub fn load(cwd: &Path) -> Self {
        if let Some(files) = tuikit::git::list_files(cwd) {
            return Self::from_files(files);
        }
        let mut files = Vec::new();
        walk(cwd, "", 0, &mut files);
        Self::from_files(files)
    }
}

fn walk(root: &Path, rel: &str, depth: usize, out: &mut Vec<String>) {
    if depth > 6 || out.len() >= 20_000 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(root.join(rel)) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name == "node_modules" || name == "target" {
            continue;
        }
        let path = if rel.is_empty() {
            name
        } else {
            format!("{rel}/{name}")
        };
        match e.file_type() {
            Ok(t) if t.is_dir() => walk(root, &path, depth + 1, out),
            Ok(t) if t.is_file() => out.push(path),
            _ => {}
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchMode {
    Results,
    FilesystemOnly,
    Tools,
}

impl SearchMode {
    fn previous(self) -> Self {
        match self {
            Self::Results => Self::Tools,
            Self::FilesystemOnly => Self::Results,
            Self::Tools => Self::FilesystemOnly,
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Results => Self::FilesystemOnly,
            Self::FilesystemOnly => Self::Tools,
            Self::Tools => Self::Results,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Results => "All Results",
            Self::FilesystemOnly => "Filesystem Only",
            Self::Tools => "Plugins",
        }
    }
}

/// What Tab or Enter picked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Picked {
    /// A path to put in the draft as text.
    Path(String),
    /// A skill name, inserted as the `$name` element.
    Skill(String),
}

#[derive(Clone, Debug)]
struct Row {
    /// Full relative path; directories end in `/`. For a skill, its name.
    path: String,
    is_dir: bool,
    /// A skill row: the tag says `Skill` and the secondary column is its description.
    skill_desc: Option<String>,
    /// Char offsets into `path`.
    match_indices: Vec<usize>,
    score: i32,
}

impl Row {
    fn trimmed(&self) -> &str {
        self.path.trim_end_matches('/')
    }

    /// Char offset where the file name starts.
    fn file_name_start(&self) -> usize {
        if self.skill_desc.is_some() {
            return 0;
        }
        let t = self.trimmed();
        t.rfind('/').map_or(0, |i| t[..i + 1].chars().count())
    }

    fn file_name(&self) -> String {
        self.trimmed()
            .chars()
            .skip(self.file_name_start())
            .collect()
    }
}

#[derive(Debug)]
pub struct MentionPopup {
    query: String,
    index: Arc<FileIndex>,
    skills: Arc<Vec<Skill>>,
    rows: Vec<Row>,
    mode: SearchMode,
    state: ScrollState,
}

impl MentionPopup {
    pub fn new(index: Arc<FileIndex>, skills: Arc<Vec<Skill>>, query: &str) -> Self {
        let mut p = MentionPopup {
            query: query.to_string(),
            index,
            skills,
            rows: Vec::new(),
            mode: SearchMode::Results,
            state: ScrollState::default(),
        };
        p.refresh();
        p
    }

    pub fn set_query(&mut self, query: &str) {
        if self.query == query {
            return;
        }
        self.query = query.to_string();
        self.refresh();
    }

    fn refresh(&mut self) {
        self.rows = search(&self.index, &self.skills, &self.query, self.mode);
        let n = self.rows.len();
        self.state.clamp_selection(n);
        self.state.ensure_visible(n, MAX_POPUP_ROWS.min(n));
    }

    pub fn move_up(&mut self) {
        let n = self.rows.len();
        self.state.move_up_wrap(n);
        self.state.ensure_visible(n, MAX_POPUP_ROWS.min(n));
    }

    pub fn move_down(&mut self) {
        let n = self.rows.len();
        self.state.move_down_wrap(n);
        self.state.ensure_visible(n, MAX_POPUP_ROWS.min(n));
    }

    pub fn previous_search_mode(&mut self) {
        self.mode = self.mode.previous();
        self.refresh();
    }

    pub fn next_search_mode(&mut self) {
        self.mode = self.mode.next();
        self.refresh();
    }

    /// What to insert for the selected row.
    pub fn selected(&self) -> Option<Picked> {
        let i = self.state.selected_idx?;
        self.rows.get(i).map(|r| match r.skill_desc {
            Some(_) => Picked::Skill(r.path.clone()),
            None => Picked::Path(r.path.clone()),
        })
    }

    /// The path of the selected file row (tests).
    pub fn selected_path(&self) -> Option<String> {
        match self.selected()? {
            Picked::Path(p) => Some(p),
            Picked::Skill(_) => None,
        }
    }

    pub fn required_height(&self, _width: u16) -> u16 {
        (self.rows.len().clamp(1, MAX_POPUP_ROWS) as u16) + 2
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let (list_area, hint_area) = if area.height > 2 {
            (
                Rect::new(area.x, area.y, area.width, area.height - 2),
                Some(Rect::new(area.x, area.y + area.height - 1, area.width, 1)),
            )
        } else {
            (area, None)
        };
        self.render_rows(list_area, buf);
        if let Some(h) = hint_area {
            let inset = FOOTER_INDENT_COLS.min(h.width);
            render_footer(
                Rect::new(h.x + inset, h.y, h.width - inset, 1),
                buf,
                self.mode,
            );
        }
    }

    fn render_rows(&self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }
        if self.rows.is_empty() {
            let l = Line::from(vec![
                Span::raw("  "),
                Span::styled("no matches", Style::default().italic()),
            ]);
            tuikit::paint::put_line(buf, area.x, area.y, &l, area);
            return;
        }
        let visible = MAX_POPUP_ROWS
            .min(self.rows.len())
            .min(area.height.max(1) as usize);
        let mut start = self.state.scroll_top.min(self.rows.len() - 1);
        if let Some(sel) = self.state.selected_idx {
            if sel < start {
                start = sel;
            } else if visible > 0 && sel > start + visible - 1 {
                start = sel + 1 - visible;
            }
        }
        let primary_w = self
            .rows
            .iter()
            .skip(start)
            .take(visible)
            .map(|r| width_of(&r.file_name()))
            .max()
            .unwrap_or(0);
        for (i, row) in self.rows.iter().enumerate().skip(start).take(visible) {
            let y = area.y + (i - start) as u16;
            if y >= area.bottom() {
                break;
            }
            let selected = Some(i) == self.state.selected_idx;
            let line = build_line(row, selected, area.width as usize, primary_w);
            tuikit::paint::put_line(buf, area.x, y, &line, Rect::new(area.x, y, area.width, 1));
        }
    }
}

fn tag_span(row: &Row) -> Span<'static> {
    let (label, style) = if row.skill_desc.is_some() {
        ("Skill", Style::default().dim())
    } else if row.is_dir {
        ("Dir", Style::default())
    } else {
        ("File", Style::default().fg(Color::Cyan))
    };
    Span::styled(format!("{label:<TAG_WIDTH$}"), style)
}

fn build_line(row: &Row, selected: bool, width: usize, primary_w: usize) -> Line<'static> {
    let dim = Style::default().dim();
    let tag = tag_span(row);
    let tag_w = width_of(&tag.content);
    let gutter = if selected { "> " } else { "  " };
    let content_width = width.saturating_sub(gutter.len() + tag_w + 2);

    // Primary: the file name in cyan for files. Secondary: the directory, `./` at the root,
    // with the matched characters of the query bold.
    let name = row.file_name();
    let mut content: Vec<Span<'static>> = vec![Span::styled(
        name.clone(),
        if row.is_dir {
            Style::default()
        } else if row.skill_desc.is_some() {
            dim
        } else {
            Style::default().fg(Color::Cyan)
        },
    )];
    let start = row.file_name_start();
    content.push(Span::styled(
        " ".repeat(primary_w.saturating_sub(width_of(&name)) + 2),
        dim,
    ));
    if let Some(desc) = &row.skill_desc {
        content.push(Span::styled(desc.clone(), dim));
    } else if start == 0 {
        content.push(Span::styled("./", dim));
    } else {
        for (ci, ch) in row.path.chars().enumerate().take(start) {
            let s = if row.match_indices.contains(&ci) {
                dim.bold()
            } else {
                dim
            };
            content.push(Span::styled(ch.to_string(), s));
        }
    }
    let content = truncate_with_ellipsis(Line::from(content), content_width);
    let used = line_width(&content);
    let mut spans = vec![Span::raw(gutter)];
    spans.extend(content.spans);
    let pad = width.saturating_sub(gutter.len() + used + tag_w);
    if pad > 0 {
        spans.push(Span::styled(" ".repeat(pad), dim));
    }
    spans.push(tag);
    if selected {
        let accent = palette().accent();
        spans.iter_mut().for_each(|s| s.style = accent);
    }
    Line::from(spans)
}

fn render_footer(area: Rect, buf: &mut Buffer, mode: SearchMode) {
    if area.width == 0 {
        return;
    }
    let right = search_mode_line(mode);
    let right_w = line_width(&right) as u16;
    let gap = u16::from(right_w > 0);
    let left_w = area.width.saturating_sub(right_w).saturating_sub(gap);
    if left_w > 0 {
        let left = truncate_with_ellipsis(footer_hint_line(), left_w as usize);
        tuikit::paint::put_line(
            buf,
            area.x,
            area.y,
            &left,
            Rect::new(area.x, area.y, left_w, 1),
        );
    }
    if right_w > 0 && right_w <= area.width {
        let x = area.x + area.width - right_w;
        tuikit::paint::put_line(buf, x, area.y, &right, Rect::new(x, area.y, right_w, 1));
    }
}

fn footer_hint_line() -> Line<'static> {
    let d = Style::default().dim();
    Line::from(vec![
        Span::styled("enter", d),
        Span::styled(" insert · ", d),
        Span::styled("esc", d),
        Span::styled(" close · ", d),
        Span::styled("←", d),
        Span::styled("/", d),
        Span::styled("→", d),
        Span::styled(" switch search modes", d),
    ])
}

fn search_mode_line(active: SearchMode) -> Line<'static> {
    let modes = [
        SearchMode::Results,
        SearchMode::FilesystemOnly,
        SearchMode::Tools,
    ];
    let mut spans = Vec::new();
    for (i, m) in modes.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ", Style::default().dim()));
        }
        if m == active {
            let label = format!("[{}]", m.label());
            let color = if m == SearchMode::Tools {
                Color::Magenta
            } else {
                Color::Cyan
            };
            spans.push(Span::styled(label, Style::default().fg(color).bold()));
        } else {
            spans.push(Span::styled(
                format!(" {} ", m.label()),
                Style::default().dim(),
            ));
        }
    }
    Line::from(spans)
}

/// Rows for `query`: best fuzzy score first, directories after files at equal score; with no
/// query, the first files in path order.
fn search(index: &FileIndex, skills: &[Skill], query: &str, mode: SearchMode) -> Vec<Row> {
    if mode == SearchMode::Tools {
        return Vec::new();
    }
    let q = query.trim();
    let skill_rows = |q: &str| -> Vec<Row> {
        if mode == SearchMode::FilesystemOnly {
            return Vec::new();
        }
        let mut rows: Vec<Row> = skills
            .iter()
            .filter_map(|s| {
                let (indices, score) = if q.is_empty() {
                    (Vec::new(), 0)
                } else {
                    let m = tuikit::fuzzy::score(q, &s.name)?;
                    (m.indices, -m.score)
                };
                Some(Row {
                    path: s.name.clone(),
                    is_dir: false,
                    skill_desc: Some(s.description.clone()),
                    match_indices: indices,
                    score,
                })
            })
            .collect();
        // Empty filter: alphabetical, which is the order they arrive in. Otherwise best first.
        if !q.is_empty() {
            rows.sort_by(|a, b| a.score.cmp(&b.score).then_with(|| a.path.cmp(&b.path)));
        }
        rows
    };
    if q.is_empty() {
        let mut rows = skill_rows(q);
        // Codex lists no files for an empty query; without any skill, files beat a blank popup.
        if rows.is_empty() && mode != SearchMode::Tools {
            rows = index
                .files
                .iter()
                .map(|f| Row {
                    path: f.clone(),
                    is_dir: false,
                    skill_desc: None,
                    match_indices: Vec::new(),
                    score: 0,
                })
                .collect();
        }
        rows.truncate(MAX_POPUP_ROWS);
        return rows;
    }
    let mut rows: Vec<Row> = Vec::new();
    let mut consider = |path: &str, is_dir: bool| {
        let shown = path.trim_end_matches('/');
        let Some(m) = tuikit::fuzzy::score(q, shown) else {
            return;
        };
        // A hit inside the file name beats the same letters spread over directories.
        let name_start = shown.rfind('/').map_or(0, |i| i + 1);
        let name = &shown[name_start..];
        let bonus = tuikit::fuzzy::score(q, name).map_or(0, |n| n.score / 2 + 20);
        rows.push(Row {
            path: path.to_string(),
            is_dir,
            skill_desc: None,
            match_indices: m.indices,
            score: m.score + bonus - i32::from(is_dir),
        });
    };
    if mode != SearchMode::Tools {
        for f in &index.files {
            consider(f, false);
        }
        for d in &index.dirs {
            consider(d, true);
        }
    }
    rows.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.path.cmp(&b.path)));
    // Skills come before files (Codex sorts plugins, skills, then files).
    let mut out = skill_rows(q);
    out.extend(rows);
    out.truncate(MAX_POPUP_ROWS);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> Arc<FileIndex> {
        Arc::new(FileIndex::from_files(
            [
                "Cargo.toml",
                "README.md",
                "notes.txt",
                "src/lib.rs",
                "src/main.rs",
            ]
            .map(String::from)
            .to_vec(),
        ))
    }

    fn no_skills() -> Arc<Vec<Skill>> {
        Arc::new(Vec::new())
    }

    fn skills() -> Arc<Vec<Skill>> {
        Arc::new(
            [
                (
                    "Image Gen",
                    "Generate or edit images for websites, games, and more",
                ),
                (
                    "Skill Installer",
                    "Install curated skills from openai/skills or other repos",
                ),
            ]
            .map(|(n, d)| Skill {
                name: n.into(),
                description: d.into(),
                path: std::path::PathBuf::new(),
            })
            .to_vec(),
        )
    }

    fn dump(p: &MentionPopup, w: u16) -> Vec<String> {
        let h = p.required_height(w);
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        p.render(area, &mut buf);
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn index_derives_directories() {
        let i = index();
        assert_eq!(i.dirs, vec!["src/".to_string()]);
    }

    #[test]
    fn at_lib_finds_one_file_row_with_tag_and_footer() {
        // start-10-at-filtered: `> lib.rs  src/` ... `File`, a blank row, the footer.
        let p = MentionPopup::new(index(), no_skills(), "lib");
        let rows = dump(&p, 120);
        assert_eq!(rows.len(), 3);
        assert!(rows[0].starts_with("> lib.rs  src/"), "{rows:?}");
        assert!(rows[0].ends_with("File"));
        assert_eq!(
            rows[0].chars().count(),
            118,
            "File is padded to six cells, so two columns stay blank"
        );
        assert_eq!(rows[1], "");
        assert!(rows[2].starts_with("  enter insert · esc close · ←/→ switch search modes"));
        assert!(rows[2].ends_with("[All Results]   Filesystem Only    Plugins"));
        assert_eq!(rows[2].chars().count(), 119);
    }

    #[test]
    fn selected_row_is_one_accent_run() {
        let p = MentionPopup::new(index(), no_skills(), "lib");
        let area = Rect::new(0, 0, 80, 3);
        let mut buf = Buffer::empty(area);
        p.render(area, &mut buf);
        let accent = palette().accent();
        for x in [0u16, 4, 20, 70, 75] {
            assert_eq!(buf[(x, 0)].fg, accent.fg.unwrap(), "x={x}");
        }
    }

    #[test]
    fn footer_is_absent_at_40_columns() {
        let p = MentionPopup::new(index(), no_skills(), "lib");
        let rows = dump(&p, 40);
        assert_eq!(rows[2], "");
        assert_eq!(rows[0].chars().count(), 38);
    }

    #[test]
    fn empty_query_lists_files_and_no_match_is_italic() {
        let p = MentionPopup::new(index(), no_skills(), "");
        assert_eq!(p.required_height(80), 7);
        let p = MentionPopup::new(index(), no_skills(), "zzzz");
        assert_eq!(dump(&p, 80)[0], "  no matches");
    }

    #[test]
    fn modes_cycle_and_tools_is_empty() {
        let mut p = MentionPopup::new(index(), no_skills(), "lib");
        p.next_search_mode();
        assert!(dump(&p, 120)[2].contains("[Filesystem Only]"));
        p.next_search_mode();
        assert_eq!(dump(&p, 120)[0], "  no matches");
        p.previous_search_mode();
        p.previous_search_mode();
        assert_eq!(p.selected_path().as_deref(), Some("src/lib.rs"));
    }

    #[test]
    fn name_match_beats_directory_match() {
        let p = MentionPopup::new(index(), no_skills(), "main");
        assert_eq!(p.selected_path().as_deref(), Some("src/main.rs"));
    }

    #[test]
    fn empty_query_lists_only_skills_like_the_capture() {
        // reference/codex/150x42/start-09-at-files: skill rows, a `Skill` tag, no files.
        let p = MentionPopup::new(index(), skills(), "");
        let rows = dump(&p, 120);
        assert_eq!(rows.len(), 4);
        assert!(
            rows[0].starts_with("> Image Gen        Generate or edit images"),
            "{rows:?}"
        );
        assert!(rows[0].ends_with("Skill"));
        assert_eq!(rows[0].chars().count(), 119);
        assert!(rows[1].starts_with("  Skill Installer  Install curated skills"));
        assert_eq!(p.selected(), Some(Picked::Skill("Image Gen".into())));
    }

    #[test]
    fn a_query_lists_skills_before_files_and_filesystem_only_drops_them() {
        let mut p = MentionPopup::new(index(), skills(), "s");
        let rows = dump(&p, 120);
        assert!(rows[0].ends_with("Skill"), "{rows:?}");
        p.next_search_mode();
        let rows = dump(&p, 120);
        assert!(rows.iter().all(|r| !r.ends_with("Skill")), "{rows:?}");
    }

    #[test]
    fn skill_rows_are_dim_unless_selected() {
        let p = MentionPopup::new(index(), skills(), "");
        let area = Rect::new(0, 0, 120, 5);
        let mut buf = Buffer::empty(area);
        p.render(area, &mut buf);
        assert!(buf[(5, 1)].modifier.contains(ratatui::style::Modifier::DIM));
        assert_eq!(buf[(5, 0)].fg, palette().accent().fg.unwrap());
    }
}
