//! `/resume` and `--resume` (spec 11.6), after `session-selector.js`. Wizard lists sessions with a
//! title (the first prompt), a cwd and an update time; it reports no message count, no names, no
//! parent links and cannot delete or rename, so the count column is left empty, the list is a
//! flat forest (threaded and recent look the same), and the `ctrl+d delete` and `ctrl+r rename`
//! hints are not shown.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use tuikit::width::display_width;

use agent_core::SessionInfo;

use super::{cut, is_cancel, is_ctrl, Input, SelOut};
use crate::theme::Tok;
use crate::ui::{rule, span, Cx, Lines};

const VISIBLE: usize = 10;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Sort {
    Threaded,
    Recent,
    Fuzzy,
}

pub struct ResumeSelector {
    sessions: Vec<SessionInfo>,
    cwd: String,
    home: String,
    current_id: String,
    now: i64,
    all_scope: bool,
    sort: Sort,
    named_only: bool,
    show_path: bool,
    input: Input,
    shown: Vec<SessionInfo>,
    sel: usize,
    touched: bool,
    loading: bool,
}

/// `now`, `5m`, `2h`, `3d`, `2w`, `4mo`, `1y`.
pub fn age(now: i64, then: i64) -> String {
    let d = (now - then).max(0);
    let (mins, hours, days) = (d / 60, d / 3600, d / 86400);
    if mins < 1 {
        "now".into()
    } else if mins < 60 {
        format!("{mins}m")
    } else if hours < 24 {
        format!("{hours}h")
    } else if days < 7 {
        format!("{days}d")
    } else if days < 30 {
        format!("{}w", days / 7)
    } else if days < 365 {
        format!("{}mo", days / 30)
    } else {
        format!("{}y", days / 365)
    }
}

fn search_text(s: &SessionInfo) -> String {
    format!("{} {} {}", s.id, s.title, s.cwd)
}

/// Pi's query language: `re:<pattern>`, `"phrase"` exact, otherwise fuzzy tokens.
fn matches(s: &SessionInfo, query: &str) -> Option<f64> {
    let q = query.trim();
    let text = search_text(s);
    if let Some(p) = q.strip_prefix("re:") {
        let re = regex::RegexBuilder::new(p.trim())
            .case_insensitive(true)
            .build()
            .ok()?;
        return re.find(&text).map(|m| m.start() as f64 * 0.1);
    }
    let mut total = 0.0;
    let mut rest = q.to_string();
    let norm = |t: &str| {
        t.to_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let ntext = norm(&text);
    // quoted phrases must appear exactly (whitespace-normalised)
    while let Some(a) = rest.find('"') {
        let Some(b) = rest[a + 1..].find('"') else {
            break;
        };
        let phrase = norm(&rest[a + 1..a + 1 + b]);
        if !phrase.is_empty() {
            total += ntext.find(&phrase)? as f64 * 0.1;
        }
        rest = format!("{} {}", &rest[..a], &rest[a + b + 2..]);
    }
    for tok in rest.split_whitespace().filter(|t| !t.contains('"')) {
        total -= tuikit::fuzzy::score(tok, &text)?.score as f64;
    }
    Some(total)
}

impl ResumeSelector {
    pub fn new(
        sessions: &[SessionInfo],
        cwd: &str,
        current_id: &str,
        home: &str,
        now: i64,
    ) -> Self {
        let mut s = ResumeSelector {
            sessions: Vec::new(),
            cwd: cwd.into(),
            home: home.into(),
            current_id: current_id.into(),
            now,
            all_scope: false,
            sort: Sort::Threaded,
            named_only: false,
            show_path: false,
            input: Input::new(),
            shown: Vec::new(),
            sel: 0,
            touched: false,
            loading: false,
        };
        s.set_sessions(sessions);
        s
    }

    /// A fresh list from the backend; the highlight stays on the same session once moved.
    /// The backend has not listed sessions yet (it can take seconds on a large home): an empty
    /// list says `Loading...` instead of claiming there are none.
    pub fn set_loading(&mut self, loading: bool) {
        self.loading = loading;
    }

    pub fn set_sessions(&mut self, sessions: &[SessionInfo]) {
        self.loading = false;
        let keep = self
            .touched
            .then(|| self.shown.get(self.sel).map(|s| s.id.clone()))
            .flatten();
        let mut v = sessions.to_vec();
        v.sort_by_key(|s| std::cmp::Reverse(s.updated));
        self.sessions = v;
        self.refilter();
        if let Some(id) = keep {
            if let Some(i) = self.shown.iter().position(|s| s.id == id) {
                self.sel = i;
            }
        } else {
            self.sel = 0;
        }
    }

    fn refilter(&mut self) {
        let q = self.input.text().to_string();
        let base: Vec<&SessionInfo> = self
            .sessions
            .iter()
            .filter(|s| self.all_scope || s.cwd.is_empty() || s.cwd == self.cwd)
            .filter(|_| !self.named_only)
            .collect();
        let mut v: Vec<SessionInfo> = base.into_iter().cloned().collect();
        if !q.trim().is_empty() {
            let mut scored: Vec<(f64, usize, SessionInfo)> = v
                .into_iter()
                .enumerate()
                .filter_map(|(i, s)| matches(&s, &q).map(|sc| (sc, i, s)))
                .collect();
            if self.sort != Sort::Recent {
                scored.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap().then(a.1.cmp(&b.1)));
            }
            v = scored.into_iter().map(|(_, _, s)| s).collect();
        }
        self.shown = v;
        self.sel = self.sel.min(self.shown.len().saturating_sub(1));
    }

    pub fn paste(&mut self, text: &str) {
        self.input.paste(&text.replace(['\n', '\r'], " "));
        self.refilter();
    }

    pub fn on_key(&mut self, key: KeyEvent) -> SelOut {
        let n = self.shown.len();
        if key.code == KeyCode::Tab {
            self.all_scope = !self.all_scope;
            self.refilter();
            return SelOut::None;
        }
        if is_ctrl(&key, 's') {
            self.sort = match self.sort {
                Sort::Threaded => Sort::Recent,
                Sort::Recent => Sort::Fuzzy,
                Sort::Fuzzy => Sort::Threaded,
            };
            self.refilter();
            return SelOut::None;
        }
        if is_ctrl(&key, 'n') {
            self.named_only = !self.named_only;
            self.refilter();
            return SelOut::None;
        }
        if is_ctrl(&key, 'p') {
            self.show_path = !self.show_path;
            return SelOut::None;
        }
        if is_ctrl(&key, 'd') || is_ctrl(&key, 'r') {
            return SelOut::None;
        }
        self.touched = true;
        match key.code {
            KeyCode::Up => self.sel = self.sel.saturating_sub(1),
            KeyCode::Down => self.sel = (self.sel + 1).min(n.saturating_sub(1)),
            KeyCode::PageUp => self.sel = self.sel.saturating_sub(VISIBLE),
            KeyCode::PageDown => self.sel = (self.sel + VISIBLE).min(n.saturating_sub(1)),
            KeyCode::Enter => {
                if let Some(s) = self.shown.get(self.sel) {
                    return SelOut::Resume(s.id.clone());
                }
            }
            _ if is_cancel(&key) => return SelOut::Close,
            _ => {
                if key.modifiers.contains(KeyModifiers::ALT) {
                    return SelOut::None;
                }
                if self.input.key(key) {
                    self.refilter();
                }
            }
        }
        SelOut::None
    }

    fn short(&self, p: &str) -> String {
        if !self.home.is_empty() && (p == self.home || p.starts_with(&format!("{}/", self.home))) {
            format!("~{}", &p[self.home.len()..])
        } else {
            p.to_string()
        }
    }

    fn header(&self, cx: &Cx) -> Lines {
        let th = cx.th();
        let (muted, accent) = (th.fg(Tok::Muted), th.fg(Tok::Accent));
        let w = cx.width as usize;
        let title = if self.all_scope {
            "Resume Session (All)"
        } else {
            "Resume Session (Current Folder)"
        };
        let (cur, all) = if self.all_scope {
            (("○ Current Folder | ", muted), ("◉ All", accent))
        } else {
            (("◉ Current Folder", accent), (" | ○ All", muted))
        };
        let sort = match self.sort {
            Sort::Threaded => "Threaded",
            Sort::Recent => "Recent",
            Sort::Fuzzy => "Fuzzy",
        };
        let name = if self.named_only { "Named" } else { "All" };
        let mut right: Vec<Span<'static>> = vec![
            span(cur.0, cur.1),
            span(all.0, all.1),
            Span::raw("  "),
            span("Name: ", muted),
            span(name, accent),
            Span::raw("  "),
            span("Sort: ", muted),
            span(sort, accent),
        ];
        right = tuikit::width::clip_spans(right, w);
        let rw = tuikit::width::spans_width(&right);
        let left = cut(title, w.saturating_sub(rw + 1));
        let lw = display_width(&left);
        let mut row = vec![span(left, Style::default().add_modifier(Modifier::BOLD))];
        row.push(Span::raw(" ".repeat(w.saturating_sub(lw + rw))));
        row.extend(right);
        let dim = th.fg(Tok::Dim);
        let sep = || span(" · ", muted);
        let key = |k: &str, d: &str| vec![span(k.to_string(), dim), span(format!(" {d}"), muted)];
        let mut h1 = key("tab", "scope");
        h1.push(sep());
        h1.push(span("re:<pattern> regex · \"phrase\" exact", muted));
        let mut h2 = key("ctrl+s", "sort");
        h2.push(sep());
        h2.extend(key("ctrl+n", "named"));
        h2.push(sep());
        h2.extend(key(
            "ctrl+p",
            &format!("path {}", if self.show_path { "(on)" } else { "(off)" }),
        ));
        vec![
            Line::from(row),
            Line::from(tuikit::width::clip_spans(h1, w)),
            Line::from(tuikit::width::clip_spans(h2, w)),
        ]
    }

    pub fn render(&self, cx: &Cx) -> Lines {
        let th = cx.th();
        let w = cx.width as usize;
        let border = rule(cx.width, th.fg(Tok::Accent));
        let mut out: Lines = vec![super::blank_line(), border.clone(), super::blank_line()];
        out.extend(self.header(cx));
        out.push(super::blank_line());
        out.push(self.input.render(cx.width));
        out.push(super::blank_line());
        let n = self.shown.len();
        if n == 0 {
            let msg = if self.loading {
                "  Loading..."
            } else if self.named_only {
                if self.all_scope {
                    "  No named sessions found. Press ctrl+n to show all."
                } else {
                    "  No named sessions in current folder. Press ctrl+n to show all, or Tab to view all."
                }
            } else if self.all_scope {
                "  No sessions found"
            } else {
                "  No sessions in current folder. Press Tab to view all."
            };
            out.push(Line::from(span(cut(msg, w), th.fg(Tok::Muted))));
        } else {
            let start = self
                .sel
                .saturating_sub(VISIBLE / 2)
                .min(n.saturating_sub(VISIBLE));
            let end = (start + VISIBLE).min(n);
            for (i, s) in self.shown.iter().enumerate().take(end).skip(start) {
                out.push(self.row(cx, s, i == self.sel));
            }
            if start > 0 || end < n {
                out.push(Line::from(span(
                    cut(&format!("  ({}/{})", self.sel + 1, n), w),
                    th.fg(Tok::Muted),
                )));
            }
        }
        out.push(super::blank_line());
        out.push(border);
        out
    }

    fn row(&self, cx: &Cx, s: &SessionInfo, selected: bool) -> Line<'static> {
        let th = cx.th();
        let w = cx.width as usize;
        let mut right = age(self.now, s.updated);
        if self.all_scope && !s.cwd.is_empty() {
            right = format!("{} {right}", self.short(&s.cwd));
        }
        if self.show_path {
            right = format!("~/.wizard/sessions/{} {right}", s.id);
        }
        let text: String = s
            .title
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect::<String>()
            .trim()
            .to_string();
        let avail = w.saturating_sub(2 + display_width(&right) + 2);
        let msg = tuikit::width::truncate(&text, avail.max(10));
        let mut msg_style = Style::default();
        if s.id == self.current_id {
            msg_style = th.fg(Tok::Accent);
        }
        if selected {
            msg_style = msg_style.add_modifier(Modifier::BOLD);
        }
        let left_w = 2 + display_width(&msg);
        let spacing = w.saturating_sub(left_w + display_width(&right)).max(1);
        let mut spans: Vec<Span<'static>> = vec![
            if selected {
                span("› ", th.fg(Tok::Accent))
            } else {
                Span::raw("  ")
            },
            span(msg, msg_style),
            Span::raw(" ".repeat(spacing)),
            span(right, th.fg(Tok::Dim)),
        ];
        if selected {
            let bg = th.bg(Tok::SelectedBg);
            spans = spans
                .into_iter()
                .map(|sp| Span::styled(sp.content, bg.patch(sp.style)))
                .collect();
        }
        Line::from(tuikit::width::clip_spans(spans, w))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::PiTheme;
    use std::time::Duration;

    fn cx() -> Cx {
        Cx {
            theme: PiTheme::dark(),
            width: 120,
            expanded: false,
            hide_thinking: false,
            out_pad: 1,
            cwd: String::new(),
            home: "/home/me".into(),
            clock: Duration::ZERO,
            version: "1.0.3".into(),
        }
    }

    fn si(id: &str, title: &str, cwd: &str, updated: i64) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            title: title.into(),
            cwd: cwd.into(),
            updated,
        }
    }

    fn t(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn ages_follow_pi() {
        assert_eq!(age(1000, 990), "now");
        assert_eq!(age(10_000, 10_000 - 300), "5m");
        assert_eq!(age(100_000, 100_000 - 7200), "2h");
        assert_eq!(age(1_000_000, 1_000_000 - 3 * 86_400), "3d");
        assert_eq!(age(10_000_000, 10_000_000 - 20 * 86_400), "2w");
    }

    #[test]
    fn header_scope_and_rows() {
        let list = vec![
            si("a", "first prompt", "/home/me/demo", 990),
            si("b", "elsewhere", "/home/me/other", 900),
        ];
        let mut s = ResumeSelector::new(&list, "/home/me/demo", "z", "/home/me", 1000);
        let rows: Vec<String> = s.render(&cx()).iter().map(t).collect();
        assert!(
            rows[3].starts_with("Resume Session (Current Folder)"),
            "{rows:#?}"
        );
        assert!(rows[3].ends_with("◉ Current Folder | ○ All  Name: All  Sort: Threaded"));
        assert!(rows
            .iter()
            .any(|r| r.starts_with("› first prompt") && r.ends_with(" now")));
        assert!(!rows.iter().any(|r| r.contains("elsewhere")));
        s.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        let rows: Vec<String> = s.render(&cx()).iter().map(t).collect();
        assert!(rows.iter().any(|r| r.contains("~/other")), "{rows:#?}");
        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            SelOut::Resume("a".into())
        );
    }

    #[test]
    fn search_filters_by_title() {
        let list = vec![si("a", "fix parser", "", 5), si("b", "add flag", "", 4)];
        let mut s = ResumeSelector::new(&list, "/x", "", "", 10);
        for c in "flag".chars() {
            s.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!(s.shown.len(), 1);
        assert_eq!(s.shown[0].id, "b");
    }
}
