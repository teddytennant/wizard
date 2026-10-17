// OWNER: history-cells
//! Hyperlink-carrying lines. Codex marks web links with OSC 8 and keeps the destination apart
//! from the visible text so width maths never sees it. A `ratatui` cell has no room for a
//! destination, so a link span carries a tag in its `underline_color` (a 24 bit id into a small
//! interner). The tag survives wrapping, patching and the buffer diff, and the two writers
//! (`term::history_insert`, `term::inline`) turn it into `ESC ] 8 ; ; URL BEL` around the cells.
//! No writer ever emits the underline colour itself, so the tag never reaches the terminal as SGR.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Mutex, OnceLock};

use ratatui::style::Color;
use ratatui::text::{Line, Span};

use crate::width::display_width;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalHyperlink {
    pub columns: Range<usize>,
    pub destination: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HyperlinkLine {
    pub line: Line<'static>,
    pub hyperlinks: Vec<TerminalHyperlink>,
}

impl HyperlinkLine {
    pub fn new(line: Line<'static>) -> Self {
        Self {
            line,
            hyperlinks: Vec::new(),
        }
    }

    pub fn width(&self) -> usize {
        self.line
            .spans
            .iter()
            .map(|s| display_width(&s.content))
            .sum()
    }

    pub fn push_span(&mut self, mut span: Span<'static>, destination: Option<&str>) {
        if let Some(dest) = destination.and_then(web_destination) {
            let start = self.width();
            self.hyperlinks.push(TerminalHyperlink {
                columns: start..start + display_width(&span.content),
                destination: dest.clone(),
            });
            span.style.underline_color = Some(tag_for(&dest));
        }
        self.line.push_span(span);
    }

    pub fn style(mut self, style: ratatui::style::Style) -> Self {
        self.line = self.line.style(style);
        self
    }
}

pub fn visible_lines(lines: Vec<HyperlinkLine>) -> Vec<Line<'static>> {
    lines.into_iter().map(|l| l.line).collect()
}

pub fn plain_hyperlink_lines(lines: Vec<Line<'static>>) -> Vec<HyperlinkLine> {
    lines.into_iter().map(HyperlinkLine::new).collect()
}

/// Tag every bare http(s) URL in the line so it is written as a link. Spans are split at the URL
/// edges; text without a URL comes back untouched.
pub fn annotate_web_urls_in_line(line: Line<'static>) -> HyperlinkLine {
    let mut out = HyperlinkLine::new(Line::default().style(line.style));
    out.line.alignment = line.alignment;
    for span in line.spans {
        let text = span.content.to_string();
        let found = find_urls(&text);
        if found.is_empty() {
            out.push_span(span, None);
            continue;
        }
        let mut at = 0;
        for r in found {
            if r.start > at {
                out.push_span(
                    Span::styled(text[at..r.start].to_string(), span.style),
                    None,
                );
            }
            let url = text[r.clone()].to_string();
            out.push_span(Span::styled(url.clone(), span.style), Some(&url));
            at = r.end;
        }
        if at < text.len() {
            out.push_span(Span::styled(text[at..].to_string(), span.style), None);
        }
    }
    out
}

/// Byte ranges of the http(s) URLs in `text`. A URL ends at whitespace; trailing sentence
/// punctuation and an unbalanced closing bracket are left out of it.
pub fn find_urls(text: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut from = 0;
    while from < text.len() {
        let rest = &text[from..];
        let Some(off) = ["https://", "http://"]
            .iter()
            .filter_map(|p| rest.find(p))
            .min()
        else {
            break;
        };
        let start = from + off;
        let mut end = text[start..]
            .find(|c: char| c.is_whitespace())
            .map_or(text.len(), |i| start + i);
        loop {
            let tail = &text[start..end];
            let Some(last) = tail.chars().last() else {
                break;
            };
            let unbalanced = |open: char, close: char| {
                last == close && tail.matches(close).count() > tail.matches(open).count()
            };
            if matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"' | '>')
                || unbalanced('(', ')')
                || unbalanced('[', ']')
                || unbalanced('{', '}')
            {
                end -= last.len_utf8();
            } else {
                break;
            }
        }
        if web_destination(&text[start..end]).is_some() {
            out.push(start..end);
        }
        from = end.max(start + 1);
    }
    out
}

type Interner = (Vec<String>, HashMap<String, u32>);

fn registry() -> &'static Mutex<Interner> {
    static R: OnceLock<Mutex<Interner>> = OnceLock::new();
    R.get_or_init(|| Mutex::new((Vec::new(), HashMap::new())))
}

/// The underline-colour tag standing for `url`. Ids start at 1 so black is never a tag.
pub fn tag_for(url: &str) -> Color {
    let mut r = registry().lock().unwrap();
    let id = match r.1.get(url) {
        Some(id) => *id,
        None => {
            r.0.push(url.to_string());
            let id = r.0.len() as u32;
            r.1.insert(url.to_string(), id);
            id
        }
    };
    Color::Rgb((id >> 16) as u8, (id >> 8) as u8, id as u8)
}

/// The destination a tag stands for.
pub fn url_of(tag: Option<Color>) -> Option<String> {
    let Some(Color::Rgb(r, g, b)) = tag else {
        return None;
    };
    let id = ((r as usize) << 16) | ((g as usize) << 8) | b as usize;
    registry()
        .lock()
        .unwrap()
        .0
        .get(id.checked_sub(1)?)
        .cloned()
}

/// OSC 8 open (`Some`) or close (`None`), BEL-terminated like Codex's `terminal_hyperlinks.rs`.
pub fn osc8(url: Option<&str>) -> String {
    format!("\x1b]8;;{}\x07", url.unwrap_or(""))
}

pub fn remap_wrapped_line(
    _source: &HyperlinkLine,
    wrapped: Vec<Line<'static>>,
) -> Vec<HyperlinkLine> {
    plain_hyperlink_lines(wrapped)
}

/// `Some` for an http or https destination with a host (what OSC 8 would be allowed to carry).
pub fn web_destination(destination: &str) -> Option<String> {
    let d: String = destination.chars().filter(|c| !c.is_control()).collect();
    let rest = d
        .strip_prefix("https://")
        .or_else(|| d.strip_prefix("http://"))?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    (!host.is_empty()).then_some(d)
}
