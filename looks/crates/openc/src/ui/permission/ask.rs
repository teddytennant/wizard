//! The question panel: what `AskUserQuestion` looks like. One question at a time, its options
//! as a list with the cursor on one, single or multiple choice, and a last row that takes a
//! typed answer. It docks where the composer is, like the permission panel, and answers with
//! `(question, answer)` pairs.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use serde_json::Value;
use tuikit::editor::{Editor, EditorStyle};
use tuikit::paint::{fill, put_str};
use tuikit::width::{display_width, spans_width};

use crate::ui::row::{cut, sp, spaces, wrap_plain, Cx};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Question {
    pub header: String,
    pub question: String,
    /// `(label, description)`.
    pub options: Vec<(String, String)>,
    pub multi: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AskOut {
    None,
    /// Every question answered: `(question, answer)` in order.
    Answer(Vec<(String, String)>),
    /// The person closed it without answering.
    Skip,
}

#[derive(Clone, Debug, Default)]
struct Pick {
    /// Chosen option indices.
    chosen: Vec<usize>,
    /// Typed answer; counts as chosen when non-empty.
    other: String,
}

impl Pick {
    fn answered(&self) -> bool {
        !self.chosen.is_empty() || !self.other.trim().is_empty()
    }
}

pub struct AskPanel {
    pub qs: Vec<Question>,
    cur: usize,
    cursor: usize,
    picks: Vec<Pick>,
    typing: Option<Editor>,
}

/// Questions out of the tool input, or `None` when it does not hold any.
pub fn parse(input: &Value) -> Option<Vec<Question>> {
    let arr = input.get("questions")?.as_array()?;
    let qs: Vec<Question> = arr
        .iter()
        .filter_map(|q| {
            let s = |k: &str| q.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            let options: Vec<(String, String)> = q
                .get("options")?
                .as_array()?
                .iter()
                .filter_map(|o| {
                    let label = o.get("label")?.as_str()?.to_string();
                    let desc = o
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    Some((label, desc))
                })
                .collect();
            Some(Question {
                header: s("header"),
                question: s("question"),
                options,
                multi: q.get("multiSelect").and_then(Value::as_bool) == Some(true),
            })
        })
        .collect();
    (!qs.is_empty()).then_some(qs)
}

impl AskPanel {
    pub fn new(qs: Vec<Question>) -> AskPanel {
        let picks = vec![Pick::default(); qs.len()];
        AskPanel {
            qs,
            cur: 0,
            cursor: 0,
            picks,
            typing: None,
        }
    }

    fn q(&self) -> &Question {
        &self.qs[self.cur]
    }

    /// Option rows plus the typed-answer row.
    fn rows(&self) -> usize {
        self.q().options.len() + 1
    }

    fn other_row(&self) -> usize {
        self.q().options.len()
    }

    pub fn title(&self, sep: &str) -> String {
        let q = self.q();
        let head = if q.header.is_empty() {
            "Question".to_string()
        } else {
            q.header.clone()
        };
        if self.qs.len() > 1 {
            format!("{head} {sep} {} of {}", self.cur + 1, self.qs.len())
        } else {
            head
        }
    }

    /// Rows the panel needs: rule, question, options, other, hint.
    pub fn height(&self, cx: &Cx, avail: usize) -> u16 {
        let q_rows = wrap_plain(&self.q().question, cx.width.saturating_sub(4)).len();
        let opts = self.rows().min(avail.saturating_sub(q_rows + 3).max(3));
        let tabs = usize::from(self.qs.len() > 1);
        (1 + tabs + q_rows + opts + 1) as u16
    }

    /// Move to the next question, or finish when it was the last.
    fn advance(&mut self) -> AskOut {
        self.typing = None;
        if let Some(next) = (0..self.qs.len())
            .map(|i| (self.cur + 1 + i) % self.qs.len())
            .find(|&i| i != self.cur && !self.picks[i].answered())
        {
            self.cur = next;
            self.cursor = 0;
            return AskOut::None;
        }
        if self.picks.iter().all(Pick::answered) {
            return AskOut::Answer(self.answers());
        }
        AskOut::None
    }

    fn answers(&self) -> Vec<(String, String)> {
        self.qs
            .iter()
            .zip(&self.picks)
            .map(|(q, p)| {
                let mut parts: Vec<String> = p
                    .chosen
                    .iter()
                    .filter_map(|&i| q.options.get(i).map(|o| o.0.clone()))
                    .collect();
                if !p.other.trim().is_empty() {
                    parts.push(p.other.trim().to_string());
                }
                (q.question.clone(), parts.join(", "))
            })
            .collect()
    }

    fn toggle(&mut self, i: usize) {
        let multi = self.q().multi;
        let p = &mut self.picks[self.cur];
        if multi {
            if let Some(at) = p.chosen.iter().position(|&c| c == i) {
                p.chosen.remove(at);
            } else {
                p.chosen.push(i);
                p.chosen.sort_unstable();
            }
        } else {
            p.chosen = vec![i];
            p.other.clear();
        }
    }

    fn start_typing(&mut self) {
        let text = self.picks[self.cur].other.clone();
        self.typing = Some(Editor::with_text(&text));
    }

    pub fn on_key(&mut self, key: KeyEvent) -> AskOut {
        if let Some(ed) = self.typing.as_mut() {
            match key.code {
                KeyCode::Esc => {
                    self.typing = None;
                }
                KeyCode::Enter => {
                    let text = ed.text().trim().to_string();
                    self.typing = None;
                    let multi = self.q().multi;
                    let p = &mut self.picks[self.cur];
                    p.other = text;
                    if !multi && p.answered() {
                        p.chosen.clear();
                        return self.advance();
                    }
                    if multi && self.picks[self.cur].answered() {
                        return self.advance();
                    }
                }
                _ => {
                    ed.apply_key(key);
                }
            }
            return AskOut::None;
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return AskOut::None;
        }
        let n = self.rows();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.cursor = (self.cursor + n - 1) % n,
            KeyCode::Down | KeyCode::Char('j') => self.cursor = (self.cursor + 1) % n,
            KeyCode::Char(c @ '1'..='9') => {
                let i = c as usize - '1' as usize;
                if i < self.q().options.len() {
                    self.cursor = i;
                    self.toggle(i);
                    if !self.q().multi {
                        return self.advance();
                    }
                }
            }
            KeyCode::Char(' ') => {
                if self.cursor == self.other_row() {
                    self.start_typing();
                } else {
                    self.toggle(self.cursor);
                    if !self.q().multi {
                        return self.advance();
                    }
                }
            }
            KeyCode::Enter => {
                if self.cursor == self.other_row() {
                    self.start_typing();
                } else if self.q().multi {
                    // Enter on an option of a multiple choice picks it if nothing is picked
                    // yet, then moves on.
                    if !self.picks[self.cur].answered() {
                        self.toggle(self.cursor);
                    }
                    return self.advance();
                } else {
                    self.toggle(self.cursor);
                    return self.advance();
                }
            }
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') if self.qs.len() > 1 => {
                self.cur = (self.cur + 1) % self.qs.len();
                self.cursor = 0;
            }
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') if self.qs.len() > 1 => {
                self.cur = (self.cur + self.qs.len() - 1) % self.qs.len();
                self.cursor = 0;
            }
            KeyCode::Esc | KeyCode::Char('n' | 'N') => return AskOut::Skip,
            _ => {}
        }
        AskOut::None
    }

    pub fn paste(&mut self, s: &str) {
        if let Some(ed) = self.typing.as_mut() {
            ed.paste(s);
        }
    }

    fn tabs(&self, cx: &Cx) -> Vec<Span<'static>> {
        let p = cx.p;
        let mut v = vec![spaces(2)];
        for (i, q) in self.qs.iter().enumerate() {
            let done = self.picks[i].answered();
            let st = if i == self.cur {
                p.s_text().add_modifier(Modifier::BOLD)
            } else {
                p.s_dim()
            };
            let name = if q.header.is_empty() {
                format!("Q{}", i + 1)
            } else {
                q.header.clone()
            };
            v.push(sp(format!("{} ", i + 1), p.s_faint()));
            v.push(sp(name, st));
            if done {
                v.push(sp(format!(" {}", cx.g.ok), p.s_ok()));
            }
            v.push(spaces(3));
        }
        v
    }

    /// Paint into `rect`; returns the cursor cell while a typed answer is being edited.
    pub fn draw(&mut self, buf: &mut Buffer, rect: Rect, cx: &Cx) -> Option<(u16, u16)> {
        let p = cx.p;
        if rect.height < 4 {
            return None;
        }
        let w = rect.width as usize;
        let mut y = rect.y;
        let line = |buf: &mut Buffer,
                    y: u16,
                    spans: &[Span<'static>],
                    bg: Option<ratatui::style::Color>| {
            if let Some(bg) = bg {
                fill(
                    buf,
                    Rect::new(rect.x, y, rect.width, 1),
                    Style::new().bg(bg),
                );
            }
            let mut x = rect.x;
            for s in spans {
                let st = match bg {
                    Some(b) => Style::new().bg(b).patch(s.style),
                    None => s.style,
                };
                x = put_str(buf, x, y, &s.content, st, rect);
            }
        };
        // Title rule.
        let title = self.title(cx.g.sep);
        let mut spans = vec![
            sp(format!("{} ", cx.g.rule), p.s_accent()),
            sp(title, p.s_accent().add_modifier(Modifier::BOLD)),
            sp(" ", p.s_accent()),
        ];
        let used = spans_width(&spans);
        spans.push(sp(cx.g.rule.repeat(w.saturating_sub(used)), p.s_accent()));
        line(buf, y, &spans, None);
        y += 1;
        if self.qs.len() > 1 {
            line(buf, y, &self.tabs(cx), None);
            y += 1;
        }
        // Question text.
        let q = self.q().clone();
        for l in wrap_plain(&q.question, w.saturating_sub(4)) {
            line(
                buf,
                y,
                &[spaces(2), sp(l, p.s_text().add_modifier(Modifier::BOLD))],
                None,
            );
            y += 1;
        }
        // Options, windowed around the cursor when the panel is short.
        let hint_y = rect.bottom() - 1;
        let room = hint_y.saturating_sub(y) as usize;
        let total = self.rows();
        let start = if total <= room {
            0
        } else {
            self.cursor
                .saturating_sub(room.saturating_sub(1))
                .min(total - room)
        };
        let label_w = q
            .options
            .iter()
            .map(|o| display_width(&o.0))
            .max()
            .unwrap_or(0)
            .max(8)
            .min(w / 3);
        let mut cursor_cell = None;
        for i in start..(start + room).min(total) {
            let on = i == self.cursor;
            let pick = &self.picks[self.cur];
            let is_other = i == self.other_row();
            let chosen = if is_other {
                !pick.other.trim().is_empty()
            } else {
                pick.chosen.contains(&i)
            };
            let radio_on = format!("({})", cx.g.bullet);
            let mark = match (q.multi, chosen) {
                (true, true) => "[x]",
                (true, false) => "[ ]",
                (false, true) => radio_on.as_str(),
                (false, false) => "( )",
            };
            let bg = on.then_some(p.raised);
            let mut spans = vec![
                spaces(1),
                sp(
                    if on { cx.g.select } else { " " },
                    p.s_accent().add_modifier(Modifier::BOLD),
                ),
                sp(mark, if chosen { p.s_ok() } else { p.s_dim() }),
                spaces(1),
            ];
            if is_other {
                if let (true, Some(ed)) = (on, self.typing.as_mut()) {
                    let lead = spans_width(&spans) as u16;
                    line(buf, y, &spans, bg);
                    let area = Rect::new(rect.x + lead, y, rect.width.saturating_sub(lead + 2), 1);
                    let st = EditorStyle {
                        text: Style::new().fg(p.text),
                        placeholder: Some(("type an answer, enter keeps it".into(), p.s_faint())),
                    };
                    cursor_cell = ed.render(area, buf, &st).cursor;
                    y += 1;
                    continue;
                }
                let typed = pick.other.trim();
                spans.push(sp(
                    if typed.is_empty() {
                        format!("Other{}", cx.g.ellipsis)
                    } else {
                        cut(typed, w.saturating_sub(12))
                    },
                    if on { p.s_text() } else { p.s_dim() },
                ));
                if typed.is_empty() {
                    spans.push(spaces(2));
                    spans.push(sp("type your own answer", p.s_faint()));
                }
            } else {
                let (label, desc) = &q.options[i];
                let lw = display_width(label);
                spans.push(sp(
                    cut(label, label_w),
                    if on {
                        p.s_text().add_modifier(Modifier::BOLD)
                    } else {
                        p.s_text()
                    },
                ));
                spans.push(spaces(label_w.saturating_sub(lw) + 2));
                let used = spans_width(&spans);
                spans.push(sp(cut(desc, w.saturating_sub(used + 1)), p.s_dim()));
            }
            line(buf, y, &spans, bg);
            y += 1;
        }
        // Hint row.
        let key = |k: &str, l: &str| -> Vec<Span<'static>> {
            vec![
                sp(k.to_string(), p.s_text().add_modifier(Modifier::BOLD)),
                sp(format!(" {l}"), p.s_dim()),
                spaces(3),
            ]
        };
        let mut hint = vec![spaces(2)];
        if self.typing.is_some() {
            hint.extend(key("enter", "keep"));
            hint.extend(key("esc", "cancel"));
        } else {
            hint.extend(key(&format!("{}{}", cx.g.up, cx.g.down), "move"));
            hint.extend(key("space", if q.multi { "pick" } else { "choose" }));
            let last = self
                .picks
                .iter()
                .enumerate()
                .all(|(i, pk)| i == self.cur || pk.answered());
            hint.extend(key("enter", if last { "send" } else { "next" }));
            if self.qs.len() > 1 {
                hint.extend(key("tab", "switch"));
            }
            hint.extend(key("esc", "skip"));
        }
        line(buf, hint_y, &hint, None);
        cursor_cell
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn input() -> Value {
        json!({"questions": [
            {"header": "Language", "question": "Which language?", "multiSelect": false,
             "options": [{"label": "Rust", "description": "fast"}, {"label": "Go", "description": "simple"}]},
            {"header": "Extras", "question": "Which extras?", "multiSelect": true,
             "options": [{"label": "Tests", "description": ""}, {"label": "Docs", "description": ""}, {"label": "CI", "description": ""}]}
        ]})
    }

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn single_then_multi_answers_come_out_in_order() {
        let mut a = AskPanel::new(parse(&input()).unwrap());
        assert_eq!(a.on_key(k(KeyCode::Down)), AskOut::None);
        assert_eq!(
            a.on_key(k(KeyCode::Enter)),
            AskOut::None,
            "moves to question 2"
        );
        assert_eq!(a.title("·"), "Extras · 2 of 2");
        a.on_key(k(KeyCode::Char(' ')));
        a.on_key(k(KeyCode::Down));
        a.on_key(k(KeyCode::Down));
        a.on_key(k(KeyCode::Char(' ')));
        let out = a.on_key(k(KeyCode::Enter));
        assert_eq!(
            out,
            AskOut::Answer(vec![
                ("Which language?".into(), "Go".into()),
                ("Which extras?".into(), "Tests, CI".into()),
            ])
        );
    }

    #[test]
    fn typed_answer_replaces_the_pick_in_a_single_choice() {
        let mut a = AskPanel::new(parse(&input()).unwrap());
        a.on_key(k(KeyCode::Char('2')));
        // Back to question 1 and answer with text instead.
        a.on_key(k(KeyCode::BackTab));
        a.on_key(k(KeyCode::Up)); // wraps to the typed row
        a.on_key(k(KeyCode::Enter));
        for c in "Zig".chars() {
            a.on_key(k(KeyCode::Char(c)));
        }
        a.on_key(k(KeyCode::Enter));
        a.on_key(k(KeyCode::Char('1')));
        let out = a.on_key(k(KeyCode::Enter));
        let AskOut::Answer(v) = out else {
            panic!("{out:?}")
        };
        assert_eq!(v[0].1, "Zig");
    }

    #[test]
    fn esc_skips_and_a_question_with_no_options_is_not_parsed() {
        let mut a = AskPanel::new(parse(&input()).unwrap());
        assert_eq!(a.on_key(k(KeyCode::Esc)), AskOut::Skip);
        assert!(parse(&json!({"questions": []})).is_none());
        assert!(parse(&json!({})).is_none());
    }
}
