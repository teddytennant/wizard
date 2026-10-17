//! pi-tui's `SelectList`: rows of `→ label  description`, a primary column sized to the widest
//! label (clamped), a scroll window centred on the selection, a `(3/29)` counter.

use ratatui::text::{Line, Span};
use tuikit::width::display_width;

use super::cut;

use crate::theme::Tok;
use crate::ui::{span, Cx, Lines};

#[derive(Clone, Debug)]
pub struct Row {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

#[derive(Clone, Debug)]
pub struct SelectList {
    pub items: Vec<Row>,
    pub selected: usize,
    pub max_visible: usize,
    pub min_col: usize,
    pub max_col: usize,
}

impl SelectList {
    pub fn new(items: Vec<Row>, max_visible: usize, min_col: usize, max_col: usize) -> Self {
        SelectList {
            items,
            selected: 0,
            max_visible: max_visible.max(1),
            min_col,
            max_col,
        }
    }

    pub fn select_value(&mut self, v: &str) {
        if let Some(i) = self.items.iter().position(|r| r.value == v) {
            self.selected = i;
        }
    }

    pub fn current(&self) -> Option<&Row> {
        self.items.get(self.selected)
    }

    pub fn up(&mut self) {
        let n = self.items.len();
        if n > 0 {
            self.selected = if self.selected == 0 {
                n - 1
            } else {
                self.selected - 1
            };
        }
    }

    pub fn down(&mut self) {
        let n = self.items.len();
        if n > 0 {
            self.selected = if self.selected + 1 == n {
                0
            } else {
                self.selected + 1
            };
        }
    }

    fn range(&self) -> (usize, usize) {
        let n = self.items.len();
        let start = self
            .selected
            .saturating_sub(self.max_visible / 2)
            .min(n.saturating_sub(self.max_visible));
        (start, (start + self.max_visible).min(n))
    }

    fn col(&self) -> usize {
        let widest = self
            .items
            .iter()
            .map(|r| display_width(&r.label) + 2)
            .max()
            .unwrap_or(0);
        let (lo, hi) = (
            self.min_col.min(self.max_col),
            self.min_col.max(self.max_col),
        );
        widest.clamp(lo.max(1), hi.max(1))
    }

    pub fn render(&self, cx: &Cx) -> Lines {
        let th = cx.th();
        let width = cx.width as usize;
        if self.items.is_empty() {
            return vec![Line::from(span(
                "  No matching commands",
                th.fg(Tok::Muted),
            ))];
        }
        let col = self.col();
        let (start, end) = self.range();
        let accent = th.fg(Tok::Accent);
        let mut out: Lines = Vec::new();
        for (i, r) in self.items.iter().enumerate().take(end).skip(start) {
            let sel = i == self.selected;
            let prefix = if sel { "→ " } else { "  " };
            let desc = r
                .description
                .as_deref()
                .map(|d| {
                    d.split(['\r', '\n'])
                        .collect::<Vec<_>>()
                        .join(" ")
                        .trim()
                        .to_string()
                })
                .filter(|d| !d.is_empty());
            let mut line: Option<Line<'static>> = None;
            if let (Some(d), true) = (&desc, width > 40) {
                let eff = col.min(width.saturating_sub(2 + 4)).max(1);
                let maxp = eff.saturating_sub(2).max(1);
                let label = cut(&r.label, maxp);
                let lw = display_width(&label);
                let spacing = " ".repeat(eff.saturating_sub(lw).max(1));
                let dstart = 2 + lw + spacing.len();
                let remaining = width as isize - dstart as isize - 2;
                if remaining > 10 {
                    let d = cut(d, remaining as usize);
                    line = Some(if sel {
                        Line::from(span(format!("{prefix}{label}{spacing}{d}"), accent))
                    } else {
                        Line::from(vec![
                            Span::raw(format!("{prefix}{label}")),
                            span(format!("{spacing}{d}"), th.fg(Tok::Muted)),
                        ])
                    });
                }
            }
            let line = line.unwrap_or_else(|| {
                let label = cut(&r.label, width.saturating_sub(4).max(1));
                if sel {
                    Line::from(span(format!("{prefix}{label}"), accent))
                } else {
                    Line::from(format!("{prefix}{label}"))
                }
            });
            out.push(line);
        }
        if start > 0 || end < self.items.len() {
            out.push(Line::from(span(
                format!("  ({}/{})", self.selected + 1, self.items.len()),
                th.fg(Tok::Muted),
            )));
        }
        out
    }
}

/// Style helper for hint rows: two spaces, `dim`.
pub fn hint(cx: &Cx, text: &str) -> Line<'static> {
    Line::from(span(format!("  {text}"), cx.th().fg(Tok::Dim)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::PiTheme;
    use std::time::Duration;

    fn cx() -> Cx {
        Cx {
            theme: PiTheme::dark(),
            width: 100,
            expanded: false,
            hide_thinking: false,
            out_pad: 1,
            cwd: String::new(),
            home: String::new(),
            clock: Duration::ZERO,
            version: "1.0.3".into(),
        }
    }

    fn t(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn rows_align_descriptions_and_count() {
        let mk = |v: &str, d: &str| Row {
            value: v.into(),
            label: v.into(),
            description: Some(d.into()),
        };
        let l = SelectList::new(
            vec![mk("off", "No reasoning"), mk("medium", "Moderate")],
            10,
            12,
            32,
        );
        let rows: Vec<String> = l.render(&cx()).iter().map(t).collect();
        assert_eq!(rows[0], "→ off         No reasoning");
        assert_eq!(rows[1], "  medium      Moderate");
    }

    #[test]
    fn counter_when_scrolling() {
        let items = (0..20)
            .map(|i| Row {
                value: i.to_string(),
                label: i.to_string(),
                description: None,
            })
            .collect();
        let l = SelectList::new(items, 5, 12, 32);
        let rows = l.render(&cx());
        assert_eq!(t(rows.last().unwrap()), "  (1/20)");
    }
}
