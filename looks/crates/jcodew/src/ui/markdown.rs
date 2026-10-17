//! Markdown to transcript lines, the way jcode lays it out: no heading marks, `•` bullets,
//! code blocks in a `┌─ │ └─` gutter with no background, tables without an outer border, and
//! one blank line after every block except a heading.

use std::sync::OnceLock;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme as SynTheme, ThemeSet};
use syntect::parsing::SyntaxSet;
use unicode_width::UnicodeWidthStr;

use super::line::{spans_width, Hang, RLine};
use crate::theme;

fn syntaxes() -> &'static SyntaxSet {
    static S: OnceLock<SyntaxSet> = OnceLock::new();
    S.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn syn_theme() -> &'static SynTheme {
    static T: OnceLock<SynTheme> = OnceLock::new();
    T.get_or_init(|| {
        let mut set = ThemeSet::load_defaults();
        set.themes.remove("base16-ocean.dark").unwrap_or_default()
    })
}

/// Load the syntax tables off the first frame.
pub fn warm() {
    let _ = syntaxes();
    let _ = syn_theme();
}

/// One highlighter per block, so multi-line constructs keep their state. `token` is a fence
/// language or a file extension.
pub struct Highlighter(Option<HighlightLines<'static>>);

impl Highlighter {
    pub fn new(token: &str) -> Self {
        let set = syntaxes();
        let syntax = set
            .find_syntax_by_token(token)
            .unwrap_or_else(|| set.find_syntax_plain_text());
        Highlighter(Some(HighlightLines::new(syntax, syn_theme())))
    }

    /// Foreground-only spans for one line of code.
    pub fn line(&mut self, text: &str) -> Vec<(String, (u8, u8, u8))> {
        let plain = || vec![(text.to_string(), (192, 197, 206))];
        let Some(h) = self.0.as_mut() else {
            return plain();
        };
        let with_nl = format!("{text}\n");
        match h.highlight_line(&with_nl, syntaxes()) {
            Ok(parts) => parts
                .into_iter()
                .map(|(st, s)| {
                    (
                        s.trim_end_matches('\n').to_string(),
                        (st.foreground.r, st.foreground.g, st.foreground.b),
                    )
                })
                .filter(|(s, _)| !s.is_empty())
                .collect(),
            Err(_) => plain(),
        }
    }
}

#[derive(Default)]
struct Inline {
    bold: usize,
    italic: usize,
    strike: usize,
    link: Option<String>,
    heading: Option<u8>,
}

struct Table {
    aligns: Vec<Alignment>,
    rows: Vec<Vec<Vec<Span<'static>>>>,
    row: Vec<Vec<Span<'static>>>,
    in_head: bool,
    head_rows: usize,
}

struct Renderer {
    width: usize,
    out: Vec<RLine>,
    spans: Vec<Span<'static>>,
    inline: Inline,
    /// One entry per open list: the next number, or `None` for bullets.
    lists: Vec<Option<u64>>,
    quote: usize,
    /// A list marker waiting for the first text of its item.
    marker: Option<Vec<Span<'static>>>,
    /// Indent of the item being written (the hang of its rows).
    item_indent: Vec<usize>,
    code: Option<(String, String)>,
    table: Option<Table>,
    /// Force every colour to this one (system notices).
    tint: Option<ratatui::style::Color>,
}

fn dim() -> Style {
    Style::default().fg(theme::md_dim())
}

impl Renderer {
    fn style(&self) -> Style {
        let mut st = Style::default().fg(theme::md_text());
        if self.inline.bold > 0 {
            st = st.fg(theme::md_bold()).add_modifier(Modifier::BOLD);
        }
        if self.inline.italic > 0 {
            st = st.add_modifier(Modifier::ITALIC);
        }
        if self.inline.strike > 0 {
            st = st.add_modifier(Modifier::CROSSED_OUT);
        }
        if self.inline.link.is_some() {
            st = st.fg(theme::md_link()).add_modifier(Modifier::UNDERLINED);
        }
        if let Some(level) = self.inline.heading {
            st = Style::default()
                .fg(theme::heading(level))
                .add_modifier(Modifier::BOLD);
            if level <= 2 {
                st = st.add_modifier(Modifier::UNDERLINED);
            }
        }
        self.tinted(st)
    }

    fn tinted(&self, st: Style) -> Style {
        match self.tint {
            Some(c) => st.fg(c),
            None => st,
        }
    }

    fn quote_prefix(&self) -> Vec<Span<'static>> {
        if self.quote == 0 {
            Vec::new()
        } else {
            vec![Span::styled("│ ".repeat(self.quote), self.tinted(dim()))]
        }
    }

    fn push_text(&mut self, text: &str, st: Style) {
        if text.is_empty() {
            return;
        }
        if let Some(t) = self.table.as_mut() {
            if let Some(cell) = t.row.last_mut() {
                cell.push(Span::styled(text.to_string(), st));
            }
            return;
        }
        if self.spans.is_empty() {
            self.spans = self.quote_prefix();
            if let Some(m) = self.marker.take() {
                self.spans.extend(m);
            } else if let Some(n) = self.item_indent.last() {
                self.spans.push(Span::raw(" ".repeat(*n)));
            }
        }
        self.spans.push(Span::styled(text.to_string(), st));
    }

    fn flush(&mut self) {
        if self.spans.is_empty() {
            return;
        }
        let mut prefix = self.quote_prefix();
        if let Some(n) = self.item_indent.last() {
            prefix.push(Span::raw(" ".repeat(*n)));
        }
        let hang = if prefix.is_empty() {
            Hang::None
        } else {
            Hang::Spans(prefix)
        };
        let spans = std::mem::take(&mut self.spans);
        self.out.push(RLine::new(spans).hang(hang));
    }

    fn blank(&mut self) {
        if self.out.last().is_some_and(|l| !l.is_blank()) {
            if self.quote > 0 {
                let p = self.quote_prefix();
                self.out.push(RLine::new(p));
            } else {
                self.out.push(RLine::blank());
            }
        }
    }

    fn top_level(&self) -> bool {
        self.lists.is_empty()
    }

    fn code_block(&mut self, lang: &str, text: &str) {
        let d = self.tinted(dim());
        let qp = self.quote_prefix();
        let with = |mut v: Vec<Span<'static>>, qp: &[Span<'static>]| {
            let mut all = qp.to_vec();
            all.append(&mut v);
            all
        };
        self.out.push(RLine::new(with(
            vec![Span::styled(format!("┌─ {lang} "), d)],
            &qp,
        )));
        let mut hl = Highlighter::new(lang);
        let gutter = with(vec![Span::styled("│ ", d)], &qp);
        for l in text.trim_end_matches('\n').split('\n') {
            let mut spans = gutter.clone();
            for (s, (r, g, b)) in hl.line(l) {
                spans.push(Span::styled(
                    s,
                    self.tinted(Style::default().fg(theme::rgb(r, g, b))),
                ));
            }
            self.out
                .push(RLine::new(spans).hang(Hang::Spans(gutter.clone())));
        }
        self.out
            .push(RLine::new(with(vec![Span::styled("└─", d)], &qp)));
    }

    fn table(&mut self, t: Table) {
        let cols = t.rows.iter().map(Vec::len).max().unwrap_or(0);
        if cols == 0 {
            return;
        }
        let mut widths = vec![1usize; cols];
        for r in &t.rows {
            for (i, c) in r.iter().enumerate() {
                widths[i] = widths[i].max(spans_width(c));
            }
        }
        let available = self.width.saturating_sub((cols - 1) * 3).max(cols);
        let floor = (available / cols).clamp(1, 5);
        while widths.iter().sum::<usize>() > available {
            let (i, w) = widths
                .iter()
                .copied()
                .enumerate()
                .max_by_key(|&(i, w)| (w, usize::MAX - i))
                .unwrap_or((0, 1));
            if w <= floor {
                break;
            }
            widths[i] -= 1;
        }
        let tc = self.tinted(Style::default().fg(theme::md_table()));
        for (ri, r) in t.rows.iter().enumerate() {
            let head = ri < t.head_rows;
            let cells: Vec<Vec<Vec<Span<'static>>>> = (0..cols)
                .map(|i| {
                    let cell = r.get(i).cloned().unwrap_or_default();
                    let cell: Vec<Span<'static>> = cell
                        .into_iter()
                        .map(|s| {
                            let st = if head {
                                Style::default()
                                    .fg(theme::md_bold())
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                s.style
                            };
                            Span::styled(s.content, self.tinted(st))
                        })
                        .collect();
                    super::line::wrap(&RLine::new(cell).hang(Hang::None), widths[i])
                })
                .collect();
            let height = cells.iter().map(Vec::len).max().unwrap_or(1);
            for h in 0..height {
                let mut spans: Vec<Span<'static>> = Vec::new();
                for (i, cell) in cells.iter().enumerate() {
                    if i > 0 {
                        spans.push(Span::styled(" │ ", tc));
                    }
                    let part = cell.get(h).cloned().unwrap_or_default();
                    let w = spans_width(&part);
                    let pad = widths[i].saturating_sub(w);
                    let (l, r) = match t.aligns.get(i) {
                        Some(Alignment::Right) => (pad, 0),
                        Some(Alignment::Center) => (pad / 2, pad - pad / 2),
                        _ => (0, pad),
                    };
                    if l > 0 {
                        spans.push(Span::raw(" ".repeat(l)));
                    }
                    spans.extend(part);
                    if r > 0 && i + 1 < cols {
                        spans.push(Span::raw(" ".repeat(r)));
                    }
                }
                self.out.push(RLine::new(spans).nowrap());
            }
            if ri + 1 == t.head_rows {
                let rule = widths
                    .iter()
                    .map(|w| "─".repeat(*w))
                    .collect::<Vec<_>>()
                    .join("─┼─");
                self.out.push(RLine::styled(rule, tc).nowrap());
            }
        }
    }

    fn event(&mut self, ev: Event<'_>) {
        match ev {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {}
                Tag::Heading { level, .. } => {
                    self.inline.heading = Some(match level {
                        HeadingLevel::H1 => 1,
                        HeadingLevel::H2 => 2,
                        HeadingLevel::H3 => 3,
                        _ => 4,
                    });
                }
                Tag::BlockQuote(_) => {
                    self.flush();
                    self.quote += 1;
                }
                Tag::CodeBlock(kind) => {
                    self.flush();
                    let lang = match kind {
                        CodeBlockKind::Fenced(l) => {
                            l.split([',', ' ']).next().unwrap_or("").to_string()
                        }
                        CodeBlockKind::Indented => String::new(),
                    };
                    self.code = Some((lang, String::new()));
                }
                Tag::List(start) => {
                    self.flush();
                    self.lists.push(start);
                }
                Tag::Item => {
                    self.flush();
                    let depth = self.lists.len().saturating_sub(1);
                    let indent = "  ".repeat(depth);
                    let marker = match self.lists.last_mut() {
                        Some(Some(n)) => {
                            let m = format!("{indent}{n}. ");
                            *n += 1;
                            m
                        }
                        _ => format!("{indent}• "),
                    };
                    self.item_indent.push(marker.width());
                    self.marker = Some(vec![Span::styled(marker, self.tinted(dim()))]);
                }
                Tag::Emphasis => self.inline.italic += 1,
                Tag::Strong => self.inline.bold += 1,
                Tag::Strikethrough => self.inline.strike += 1,
                Tag::Link { dest_url, .. } => self.inline.link = Some(dest_url.to_string()),
                Tag::Image { dest_url, .. } => {
                    let st = self.tinted(dim());
                    self.push_text("[image: ", st);
                    self.inline.link = Some(format!("\u{0}{dest_url}"));
                }
                Tag::Table(aligns) => {
                    self.flush();
                    self.table = Some(Table {
                        aligns,
                        rows: Vec::new(),
                        row: Vec::new(),
                        in_head: false,
                        head_rows: 0,
                    });
                }
                Tag::TableHead => {
                    if let Some(t) = self.table.as_mut() {
                        t.in_head = true;
                        t.row.clear();
                    }
                }
                Tag::TableRow => {
                    if let Some(t) = self.table.as_mut() {
                        t.row.clear();
                    }
                }
                Tag::TableCell => {
                    if let Some(t) = self.table.as_mut() {
                        t.row.push(Vec::new());
                    }
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => {
                    self.flush();
                    self.blank();
                }
                TagEnd::Heading(_) => {
                    self.flush();
                    self.inline.heading = None;
                }
                TagEnd::BlockQuote(_) => {
                    self.flush();
                    while self
                        .out
                        .last()
                        .is_some_and(|l| l.text().trim_end() == "│ ".repeat(self.quote).trim_end())
                    {
                        self.out.pop();
                    }
                    self.quote = self.quote.saturating_sub(1);
                    self.blank();
                }
                TagEnd::CodeBlock => {
                    if let Some((lang, text)) = self.code.take() {
                        self.code_block(&lang, &text);
                    }
                    self.blank();
                }
                TagEnd::List(_) => {
                    self.flush();
                    self.lists.pop();
                    if self.top_level() {
                        self.blank();
                    }
                }
                TagEnd::Item => {
                    self.flush();
                    self.marker = None;
                    self.item_indent.pop();
                }
                TagEnd::Emphasis => self.inline.italic = self.inline.italic.saturating_sub(1),
                TagEnd::Strong => self.inline.bold = self.inline.bold.saturating_sub(1),
                TagEnd::Strikethrough => self.inline.strike = self.inline.strike.saturating_sub(1),
                TagEnd::Link => {
                    if let Some(url) = self.inline.link.take() {
                        let st = self.tinted(dim());
                        self.push_text(&format!(" ({url})"), st);
                    }
                }
                TagEnd::Image => {
                    if let Some(url) = self.inline.link.take() {
                        let st = self.tinted(dim());
                        self.push_text(&format!("] ({})", url.trim_start_matches('\u{0}')), st);
                    }
                }
                TagEnd::TableHead => {
                    if let Some(t) = self.table.as_mut() {
                        let row = std::mem::take(&mut t.row);
                        t.rows.push(row);
                        t.head_rows = t.rows.len();
                        t.in_head = false;
                    }
                }
                TagEnd::TableRow => {
                    if let Some(t) = self.table.as_mut() {
                        let row = std::mem::take(&mut t.row);
                        t.rows.push(row);
                    }
                }
                TagEnd::Table => {
                    if let Some(t) = self.table.take() {
                        self.table(t);
                    }
                    self.blank();
                }
                _ => {}
            },
            Event::Text(t) => {
                if let Some((_, code)) = self.code.as_mut() {
                    code.push_str(&t);
                } else {
                    let st = if self
                        .inline
                        .link
                        .as_ref()
                        .is_some_and(|l| l.starts_with('\u{0}'))
                    {
                        self.tinted(dim())
                    } else {
                        self.style()
                    };
                    self.push_text(&t, st);
                }
            }
            Event::Code(t) => {
                let st = self.tinted(
                    Style::default()
                        .fg(theme::md_code_fg())
                        .bg(theme::md_code_bg()),
                );
                self.push_text(&t, st);
            }
            Event::SoftBreak => {
                if self.quote > 0 {
                    self.flush();
                } else {
                    let st = self.style();
                    self.push_text(" ", st);
                }
            }
            Event::HardBreak => self.flush(),
            Event::Rule => {
                self.flush();
                let st = self.tinted(dim());
                self.out
                    .push(RLine::styled("─".repeat(self.width), st).nowrap());
                self.blank();
            }
            Event::TaskListMarker(done) => {
                let st = self.tinted(dim());
                self.push_text(if done { "[x] " } else { "[ ] " }, st);
            }
            Event::Html(t) | Event::InlineHtml(t) => {
                let st = self.tinted(
                    Style::default()
                        .fg(theme::muted())
                        .add_modifier(Modifier::ITALIC),
                );
                self.push_text(t.trim_end_matches('\n'), st);
            }
            _ => {}
        }
    }
}

fn options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_SMART_PUNCTUATION
}

fn run(text: &str, width: usize, tint: Option<ratatui::style::Color>) -> Vec<RLine> {
    let mut r = Renderer {
        width: width.max(1),
        out: Vec::new(),
        spans: Vec::new(),
        inline: Inline::default(),
        lists: Vec::new(),
        quote: 0,
        marker: None,
        item_indent: Vec::new(),
        code: None,
        table: None,
        tint,
    };
    for ev in Parser::new_ext(text, options()) {
        r.event(ev);
    }
    r.flush();
    // a fence still open at the end of a stream
    if let Some((lang, code)) = r.code.take() {
        r.code_block(&lang, &code);
    }
    while r.out.last().is_some_and(RLine::is_blank) {
        r.out.pop();
    }
    r.out
}

/// Assistant text. `width` sizes tables and rules; paragraphs are wrapped later.
pub fn render(text: &str, width: usize) -> Vec<RLine> {
    run(text, width, None)
}

/// The same layout with every colour replaced, for system notices.
pub fn render_tinted(text: &str, width: usize, color: ratatui::style::Color) -> Vec<RLine> {
    run(text, width, Some(color))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(md: &str) -> Vec<String> {
        render(md, 40).iter().map(RLine::text).collect()
    }

    #[test]
    fn blocks_are_separated_except_after_headings() {
        assert_eq!(
            text("# Title\n\npara one\n\npara two"),
            ["Title", "para one", "", "para two"]
        );
    }

    #[test]
    fn lists_use_bullets_and_numbers() {
        assert_eq!(
            text("- a\n- b\n  - c\n\n1. x\n2. y"),
            ["• a", "• b", "  • c", "", "1. x", "2. y"]
        );
    }

    #[test]
    fn code_blocks_get_a_gutter() {
        assert_eq!(
            text("```rust\nfn main() {}\n```"),
            ["┌─ rust ", "│ fn main() {}", "└─"]
        );
    }

    #[test]
    fn tables_have_no_outer_border() {
        assert_eq!(
            text("| a | b |\n|---|---|\n| 1 | 22 |"),
            ["a │ b", "──┼───", "1 │ 22"]
        );
    }

    #[test]
    fn links_show_their_target() {
        assert_eq!(text("[docs](https://x.y)"), ["docs (https://x.y)"]);
    }
}
