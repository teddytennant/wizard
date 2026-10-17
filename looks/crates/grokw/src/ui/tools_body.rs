// OWNER: tools (open bodies of read, list, search, web search and fetch; member rows of a group)
//! What wizard's read, list, search and web tools print, read back into the pieces Grok Build's
//! blocks show: line ranges, match counts, site counts. Wizard's formats (from its own tool
//! sources): `read_file` numbers lines as `{:>6}\t{text}`; `search_files` is ripgrep's
//! `path:line:text`; `list_files` is one name per line, directories with a trailing `/`;
//! `web_search` is `N. [title](url)` with the snippet indented under it; `web_fetch` is the page
//! text. The open forms follow spec 3.6.5; the collapsed one-liners follow `74b-viewer`.

use agent_core::{ToolCall, ToolStatus};
use ratatui::style::Style;
use ratatui::text::Span;
use tuikit::width::{display_width, wrap, wrap_spans, WrapMode};

use super::syntax;
use super::tools::{input_str, path_of, rel_path, running};
use super::transcript::{Entry, EntryKey, Geo, Kind, Rail, Row};
use super::{bold, st};
use crate::theme::Theme;

// ---- parsing -----------------------------------------------------------------------------

/// A `read_file` result.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReadOut {
    pub lines: Vec<(usize, String)>,
    pub empty: bool,
    /// Lines in the file, when wizard said (it does when the read was cut short).
    pub total: Option<usize>,
    pub image: bool,
}

pub fn parse_read(out: &str) -> ReadOut {
    let mut r = ReadOut::default();
    if out.trim() == "(empty file)" {
        r.empty = true;
        return r;
    }
    for l in out.lines() {
        if let Some((n, text)) = l.split_once('\t') {
            if let Ok(n) = n.trim().parse::<usize>() {
                r.lines.push((n, text.to_string()));
                continue;
            }
        }
        if let Some(rest) = l.strip_prefix("... [showing ") {
            // `... [showing 2000 of 5000 requested lines; total 6000 lines ...`
            r.total = rest
                .split("total ")
                .nth(1)
                .and_then(|t| t.split_whitespace().next())
                .and_then(|t| t.parse().ok());
        }
    }
    r.image = r.lines.is_empty() && out.contains(" bytes") && out.contains('x') && {
        out.lines()
            .next()
            .is_some_and(|l| l.contains("PNG") || l.contains("JPEG") || l.contains("WebP"))
    };
    r
}

/// The ` (a-b)` / ` (a-b of N)` / ` (empty)` / ` (image)` after a path, when there is something to
/// say.
pub fn read_suffix(c: &ToolCall) -> String {
    let Some(out) = c.output.as_deref() else {
        return String::new();
    };
    let r = parse_read(out);
    if r.empty {
        return " (empty)".into();
    }
    if r.image {
        return " (image)".into();
    }
    let ranged = c.input.get("start_line").is_some() || c.input.get("end_line").is_some();
    match (r.lines.first(), r.lines.last()) {
        (Some((a, _)), Some((b, _))) if ranged || r.total.is_some() => match r.total {
            Some(t) => format!(" ({a}-{b} of {t})"),
            None => format!(" ({a}-{b})"),
        },
        _ => String::new(),
    }
}

/// Matches of a `search_files` result, grouped by file in the order ripgrep printed them.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SearchOut {
    pub files: Vec<(String, Vec<(usize, String)>)>,
    pub matches: usize,
}

fn split_hit(l: &str) -> Option<(&str, usize, &str)> {
    for (i, _) in l.match_indices(':') {
        let rest = &l[i + 1..];
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if digits > 0 && rest[digits..].starts_with(':') {
            let n = rest[..digits].parse().ok()?;
            return Some((&l[..i], n, &rest[digits + 1..]));
        }
    }
    None
}

pub fn parse_search(out: &str) -> SearchOut {
    let mut s = SearchOut::default();
    for l in out.lines() {
        let Some((path, n, text)) = split_hit(l) else {
            continue;
        };
        let path = path.strip_prefix("./").unwrap_or(path);
        match s.files.last_mut() {
            Some((p, v)) if p == path => v.push((n, text.to_string())),
            _ => s
                .files
                .push((path.to_string(), vec![(n, text.to_string())])),
        }
        s.matches += 1;
    }
    s
}

/// Entries of a `list_files` result.
pub fn parse_list(out: &str) -> Vec<String> {
    if out.trim() == "(empty directory)" || out.starts_with("No files matching") {
        return Vec::new();
    }
    out.lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with("... ["))
        .map(str::to_string)
        .collect()
}

/// Results of a `web_search`: `(title, url, snippet)`.
pub fn parse_web(out: &str) -> Vec<(String, String, String)> {
    let mut v: Vec<(String, String, String)> = Vec::new();
    for l in out.lines() {
        let head = l
            .split_once(". [")
            .filter(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        if let Some((_, rest)) = head {
            if let Some((title, tail)) = rest.split_once("](") {
                let url = tail.trim_end_matches(')').to_string();
                v.push((title.to_string(), url, String::new()));
                continue;
            }
        }
        if let Some(last) = v.last_mut() {
            if !l.trim().is_empty() {
                if !last.2.is_empty() {
                    last.2.push(' ');
                }
                last.2.push_str(l.trim());
            }
        }
    }
    v
}

/// The host of a URL, for the `Sources:` line.
fn host(url: &str) -> &str {
    let u = url.split("://").nth(1).unwrap_or(url);
    u.split('/').next().unwrap_or(u)
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Distinct URLs across every web search of a group, which is the count its label shows.
pub fn distinct_sites(calls: &[&ToolCall]) -> usize {
    let mut seen: Vec<String> = Vec::new();
    for c in calls {
        for (_, u, _) in parse_web(c.output.as_deref().unwrap_or("")) {
            if !seen.contains(&u) {
                seen.push(u);
            }
        }
    }
    seen.len()
}

// ---- collapsed member rows -------------------------------------------------------------------

/// The pieces of a collapsed row: label, the coloured detail and the count after it.
pub struct Line {
    pub label: String,
    pub spans: Vec<(String, Pal)>,
    pub count: String,
}

#[derive(Clone, Copy)]
pub enum Pal {
    Plain,
    Pattern,
    Path,
    Url,
}

pub fn member_line(c: &ToolCall, class: super::tools::Class) -> Line {
    use super::tools::Class;
    let out = c.output.as_deref();
    match class {
        Class::Read => Line {
            label: "Read".into(),
            spans: vec![(path_of(c), Pal::Path)],
            count: read_suffix(c),
        },
        Class::List => Line {
            label: "List".into(),
            spans: vec![(path_of(c), Pal::Path)],
            count: out
                .map(|o| format!(" ({})", plural(parse_list(o).len(), "entry", "entries")))
                .unwrap_or_default(),
        },
        Class::Search => {
            let mut spans = Vec::new();
            match input_str(c, &["pattern", "query"]) {
                Some(p) => spans.push((format!("\"{p}\""), Pal::Pattern)),
                None => spans.push((c.title.clone(), Pal::Plain)),
            }
            if let Some(g) = input_str(c, &["glob"]) {
                spans.push((" in ".into(), Pal::Plain));
                spans.push((g.to_string(), Pal::Path));
            }
            if let Some(d) = input_str(c, &["path"]) {
                spans.push((" in ".into(), Pal::Plain));
                spans.push((d.to_string(), Pal::Path));
            }
            Line {
                label: "Search".into(),
                spans,
                count: out
                    .map(|o| format!(" ({})", plural(parse_search(o).matches, "match", "matches")))
                    .unwrap_or_default(),
            }
        }
        Class::WebSearch => Line {
            label: "Web Search".into(),
            spans: vec![(
                input_str(c, &["query"]).unwrap_or(&c.title).to_string(),
                Pal::Plain,
            )],
            count: out
                .map(|o| format!(" ({})", plural(parse_web(o).len(), "site", "sites")))
                .unwrap_or_default(),
        },
        Class::Subagent => {
            // `completed in 6.4s: “Count lines”`: how it ended, then what it was asked
            let secs =
                c.input.get("grokw_secs").and_then(|v| v.as_f64()).map(|s| {
                    super::transcript::fmt_duration(std::time::Duration::from_secs_f64(s))
                });
            let state = match (c.status, secs) {
                (ToolStatus::Completed, Some(d)) => format!("completed in {d}: "),
                (ToolStatus::Failed, Some(d)) => format!("failed in {d}: "),
                (ToolStatus::Completed, None) => "completed: ".to_string(),
                (ToolStatus::Failed, None) => "failed: ".to_string(),
                _ => "running: ".to_string(),
            };
            let what =
                input_str(c, &["description", "task", "prompt", "subagent"]).unwrap_or(&c.title);
            let what = what.lines().next().unwrap_or("");
            Line {
                label: "Subagent".into(),
                spans: vec![(format!("{state}“{what}”"), Pal::Plain)],
                count: String::new(),
            }
        }
        Class::Fetch => Line {
            label: "Fetch".into(),
            spans: vec![(
                input_str(c, &["url"]).unwrap_or(&c.title).to_string(),
                Pal::Url,
            )],
            count: String::new(),
        },
        _ => Line {
            label: super::tools::titlecase(&c.name),
            spans: vec![(c.title.clone(), Pal::Plain)],
            count: String::new(),
        },
    }
}

fn pal_color(p: Pal, th: &Theme, selected: bool) -> ratatui::style::Color {
    if !selected {
        return th.gray;
    }
    match p {
        Pal::Plain => th.text_primary,
        Pal::Pattern => th.accent_success,
        Pal::Path => th.path,
        Pal::Url => th.warning,
    }
}

/// One member of an open group: bullet, bold label, detail, count. Unselected rows are all muted
/// grey; a selected one gets its colours back, the `›` bullet and the panel background.
pub fn tool_member_row(
    c: &ToolCall,
    class: super::tools::Class,
    geo: &Geo,
    th: &Theme,
    selected: bool,
) -> Row {
    let l = member_line(c, class);
    let bg = |s: Style| if selected { s.bg(th.bg_dark) } else { s };
    let mut r = Row::new();
    let glyph = if selected { "› " } else { "◆ " };
    r = if running(c) {
        r.wave(geo.cx, "◆", st(th.accent_running), th.accent_running)
            .seg(geo.cx + 1, " ", bg(st(th.accent_running)))
    } else {
        r.seg(geo.cx, glyph, bg(st(th.gray)))
    };
    let label_color = if selected { th.text_primary } else { th.gray };
    r = r.seg(geo.cx + 2, l.label.clone(), bg(bold(label_color)));
    let mut x = geo.cx + 2 + display_width(&l.label) as u16 + 1;
    let room = geo
        .cw
        .saturating_sub(4 + display_width(&l.label) + display_width(&l.count));
    let mut left = room;
    for (text, pal) in &l.spans {
        let t = tuikit::width::truncate(text.lines().next().unwrap_or(""), left);
        left = left.saturating_sub(display_width(&t));
        let color = if selected && matches!(pal, Pal::Plain) && text.starts_with(" in ") {
            th.text_primary
        } else {
            pal_color(*pal, th, selected)
        };
        let w = display_width(&t) as u16;
        r = r.seg(x, t, bg(st(color)));
        x += w;
    }
    if !l.count.is_empty() {
        r = r.seg(x, l.count.clone(), bg(st(th.gray_dim)));
    }
    r
}

// ---- open bodies -------------------------------------------------------------------------------

fn rail_of(c: &ToolCall, th: &Theme) -> Rail {
    if running(c) {
        Rail::Wave(th.accent_running)
    } else if c.status == ToolStatus::Failed {
        Rail::Solid(th.accent_error)
    } else {
        Rail::Solid(th.gray_bright)
    }
}

/// `◆ Label detail count` on the rail, label bold primary.
fn open_header(
    c: &ToolCall,
    geo: &Geo,
    th: &Theme,
    rail: Rail,
    l: &Line,
    cwd: &std::path::Path,
) -> Row {
    let acc = match rail {
        Rail::Wave(a) | Rail::Solid(a) => a,
        Rail::None => th.gray_bright,
    };
    let mut r = Row::new().rail(rail);
    r = if running(c) {
        r.wave(geo.cx, "◆", st(acc), acc)
            .seg(geo.cx + 1, " ", st(acc))
    } else {
        r.seg(geo.cx, "◆ ", st(acc))
    };
    r = r.seg(geo.cx + 2, l.label.clone(), bold(th.text_primary));
    let mut x = geo.cx + 2 + display_width(&l.label) as u16 + 1;
    let room = geo
        .cw
        .saturating_sub(4 + display_width(&l.label) + display_width(&l.count));
    let mut left = room;
    for (text, pal) in &l.spans {
        // open paths are relative to the working directory when they are inside it
        let shown = if matches!(pal, Pal::Path) {
            rel_path(text, cwd)
        } else {
            text.clone()
        };
        let t = tuikit::width::truncate(shown.lines().next().unwrap_or(""), left);
        left = left.saturating_sub(display_width(&t));
        let color = if matches!(pal, Pal::Plain) && text.starts_with(" in ") {
            th.text_primary
        } else {
            pal_color(*pal, th, true)
        };
        let w = display_width(&t) as u16;
        r = r.seg(x, t, st(color));
        x += w;
    }
    if !l.count.is_empty() {
        r = r.seg(x, l.count.clone(), st(th.gray_dim));
    }
    r
}

/// A row of the `#1c1c1c` body box.
fn boxed(geo: &Geo, th: &Theme, rail: Rail) -> Row {
    Row::new()
        .rail(rail)
        .fill(geo.cx, geo.cx + geo.cw as u16, th.bg_dark)
}

fn finish(key: EntryKey, mut rows: Vec<Row>, c: &ToolCall) -> Entry {
    for (i, r) in rows.iter_mut().enumerate() {
        r.brow = i;
    }
    Entry {
        key,
        kind: Kind::Tool,
        rows,
        groupable: true,
        collapsed: false,
        selectable: true,
        foldable: true,
        running: running(c),
        members: Vec::new(),
    }
}

/// The open form of a read, list, search, web search or fetch call.
pub fn open_entry(
    key: EntryKey,
    c: &ToolCall,
    class: super::tools::Class,
    geo: &Geo,
    th: &Theme,
    cwd: &std::path::Path,
) -> Entry {
    use super::tools::Class;
    let rail = rail_of(c, th);
    let mut line = member_line(c, class);
    let out = c.output.clone().unwrap_or_default();
    let mut rows: Vec<Row> = Vec::new();
    let bw = geo.cw.saturating_sub(2).max(10);
    match class {
        Class::Read => {
            let r = parse_read(&out);
            rows.push(open_header(c, geo, th, rail, &line, cwd));
            if c.status == ToolStatus::Failed || (r.lines.is_empty() && !r.empty && !out.is_empty())
            {
                rows.push(Row::new().rail(rail));
                for l in out.lines().take(6) {
                    for w in wrap(l, bw) {
                        rows.push(Row::new().rail(rail).seg(geo.cx, w, st(th.accent_error)));
                    }
                }
            } else if !r.lines.is_empty() {
                rows.push(Row::new().rail(rail));
                let lang = syntax::lang_for_path(&path_of(c)).to_string();
                let text: String = r
                    .lines
                    .iter()
                    .map(|(_, t)| t.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                let hl = syntax::highlight(th.syntax(), &lang, &text, th.text_primary);
                let last = r.lines.last().map_or(1, |l| l.0);
                let w = last.to_string().len();
                let body_x = geo.cx + w as u16 + 2;
                let room = (geo.cw).saturating_sub(w + 2).max(8);
                for (i, (n, _)) in r.lines.iter().enumerate() {
                    let spans: Vec<Span<'static>> = hl
                        .get(i)
                        .cloned()
                        .unwrap_or_default()
                        .into_iter()
                        .map(|s| {
                            Span::styled(s.content.replace('\t', "    "), s.style.bg(th.bg_dark))
                        })
                        .collect();
                    let wrapped = if spans.is_empty() {
                        vec![Vec::new()]
                    } else {
                        wrap_spans(&spans, room, WrapMode::Word)
                    };
                    for (j, rs) in wrapped.into_iter().enumerate() {
                        let mut row = boxed(geo, th, rail);
                        if j == 0 {
                            row = row.seg(
                                geo.cx,
                                format!("{n:>w$}  "),
                                st(th.gray_dim).bg(th.bg_dark),
                            );
                        }
                        rows.push(row.spans(body_x, &rs));
                    }
                }
            }
        }
        Class::List => {
            let entries = parse_list(&out);
            line.count = format!(" ({})", plural(entries.len(), "entry", "entries"));
            rows.push(open_header(c, geo, th, rail, &line, cwd));
            rows.push(Row::new().rail(rail));
            for e in entries {
                for w in wrap(&e, bw) {
                    rows.push(boxed(geo, th, rail).seg(
                        geo.cx + 2,
                        w,
                        st(th.text_primary).bg(th.bg_dark),
                    ));
                }
            }
        }
        Class::Search => {
            let s = parse_search(&out);
            line.count = format!(
                " ({} in {})",
                plural(s.matches, "match", "matches"),
                plural(s.files.len(), "file", "files")
            );
            rows.push(open_header(c, geo, th, rail, &line, cwd));
            rows.push(Row::new().rail(rail));
            let pat = input_str(c, &["pattern", "query"]).unwrap_or("");
            rows.push(boxed(geo, th, rail).seg(
                geo.cx + 2,
                format!("mode: {pat}"),
                st(th.gray).bg(th.bg_dark),
            ));
            rows.push(boxed(geo, th, rail));
            if s.files.is_empty() {
                rows.push(boxed(geo, th, rail).seg(
                    geo.cx + 2,
                    "(no results)",
                    st(th.gray).bg(th.bg_dark),
                ));
            }
            for (path, hits) in &s.files {
                rows.push(boxed(geo, th, rail).seg(
                    geo.cx + 2,
                    rel_path(path, cwd),
                    st(th.path).bg(th.bg_dark),
                ));
                for (n, t) in hits {
                    let lead = format!("{n:>4}  ");
                    let room = geo.cw.saturating_sub(4 + lead.len()).max(8);
                    for (k, w) in wrap(&t.replace('\t', "    "), room).into_iter().enumerate() {
                        let mut row = boxed(geo, th, rail);
                        if k == 0 {
                            row = row.seg(geo.cx + 4, lead.clone(), st(th.gray_dim).bg(th.bg_dark));
                        }
                        rows.push(row.seg(
                            geo.cx + 4 + lead.len() as u16,
                            w,
                            st(th.text_primary).bg(th.bg_dark),
                        ));
                    }
                }
            }
        }
        Class::WebSearch => {
            let results = parse_web(&out);
            line.count = format!(" ({})", plural(results.len(), "site", "sites"));
            rows.push(open_header(c, geo, th, rail, &line, cwd));
            if !out.is_empty() {
                rows.push(Row::new().rail(rail));
                for l in out.lines() {
                    for w in wrap(&l.replace('\t', "    "), bw) {
                        rows.push(boxed(geo, th, rail).seg(
                            geo.cx + 2,
                            w,
                            st(th.text_primary).bg(th.bg_dark),
                        ));
                    }
                }
                if !results.is_empty() {
                    let mut hosts: Vec<&str> = Vec::new();
                    for (_, u, _) in &results {
                        if !hosts.contains(&host(u)) {
                            hosts.push(host(u));
                        }
                    }
                    let shown = hosts.iter().take(3).copied().collect::<Vec<_>>().join(", ");
                    let more = hosts.len().saturating_sub(3);
                    let mut t = format!("Sources: {shown}");
                    if more > 0 {
                        t.push_str(&format!(" (+{more} more)"));
                    }
                    rows.push(boxed(geo, th, rail).seg(geo.cx + 2, t, st(th.gray).bg(th.bg_dark)));
                }
            }
        }
        _ => {
            // fetch: the URL, how much came back, then the page
            rows.push(open_header(c, geo, th, rail, &line, cwd));
            if !out.is_empty() {
                rows.push(Row::new().rail(rail));
                rows.push(boxed(geo, th, rail).seg(
                    geo.cx + 2,
                    format!("size: {} bytes", out.len()),
                    st(th.gray).bg(th.bg_dark),
                ));
                rows.push(boxed(geo, th, rail));
                for l in out.lines() {
                    for w in wrap(&l.replace('\t', "    "), bw) {
                        rows.push(boxed(geo, th, rail).seg(
                            geo.cx + 2,
                            w,
                            st(th.text_primary).bg(th.bg_dark),
                        ));
                    }
                }
            }
        }
    }
    finish(key, rows, c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_output_is_numbered_lines_and_a_range() {
        let r = parse_read("     3\tfn main() {\n     4\t    run();\n     5\t}");
        assert_eq!(r.lines.len(), 3);
        assert_eq!(r.lines[0], (3, "fn main() {".to_string()));
        assert!(parse_read("(empty file)").empty);
        let cut = parse_read("     1\ta\n... [showing 2000 of 5000 requested lines; total 6000 lines — use start_line/end_line to read more]");
        assert_eq!(cut.total, Some(6000));
    }

    #[test]
    fn the_range_shows_only_when_there_is_one() {
        let mut c = ToolCall {
            output: Some("    10\tx\n    11\ty".into()),
            ..Default::default()
        };
        assert_eq!(read_suffix(&c), "");
        c.input = serde_json::json!({"start_line": 10, "end_line": 11});
        assert_eq!(read_suffix(&c), " (10-11)");
        c.output = Some("(empty file)".into());
        assert_eq!(read_suffix(&c), " (empty)");
    }

    #[test]
    fn search_output_groups_by_file_and_counts_matches() {
        let s = parse_search(
            "./src/main.rs:2:    println!(\"hi\");\nsrc/main.rs:9:    println!(\"bye\");\nsrc/lib.rs:1:// println\n",
        );
        assert_eq!(s.matches, 3);
        assert_eq!(s.files.len(), 2);
        assert_eq!(s.files[0].0, "src/main.rs");
        assert_eq!(s.files[0].1[1], (9, "    println!(\"bye\");".to_string()));
        assert_eq!(parse_search("No matches for pattern 'x'.").matches, 0);
    }

    #[test]
    fn list_and_web_results_parse() {
        assert_eq!(parse_list("src/\nREADME.md"), vec!["src/", "README.md"]);
        assert!(parse_list("(empty directory)").is_empty());
        let w = parse_web(
            "1. [Ratatui](https://ratatui.rs)\n   A Rust library.\n2. [crate](https://crates.io/crates/ratatui)",
        );
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].1, "https://ratatui.rs");
        assert_eq!(w[0].2, "A Rust library.");
        assert_eq!(host(&w[1].1), "crates.io");
    }
}
