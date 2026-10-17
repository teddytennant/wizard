// OWNER: history-cells
//! Small `Line` helpers shared by the cell renderers.

use ratatui::text::{Line, Span};

/// Clone a borrowed line into an owned `'static` one.
pub fn line_to_static(line: &Line<'_>) -> Line<'static> {
    Line {
        style: line.style,
        alignment: line.alignment,
        spans: line
            .spans
            .iter()
            .map(|s| Span {
                style: s.style,
                content: std::borrow::Cow::Owned(s.content.to_string()),
            })
            .collect(),
    }
}

pub fn push_owned_lines(src: &[Line<'_>], out: &mut Vec<Line<'static>>) {
    out.extend(src.iter().map(line_to_static));
}

/// First line gets `initial_prefix`, every other line `subsequent_prefix`.
pub fn prefix_lines(
    lines: Vec<Line<'static>>,
    initial_prefix: Span<'static>,
    subsequent_prefix: Span<'static>,
) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .enumerate()
        .map(|(i, l)| {
            let mut spans = Vec::with_capacity(l.spans.len() + 1);
            spans.push(if i == 0 {
                initial_prefix.clone()
            } else {
                subsequent_prefix.clone()
            });
            spans.extend(l.spans);
            Line::from(spans).style(l.style)
        })
        .collect()
}

/// Text of a line without its styles.
pub fn line_text(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}
