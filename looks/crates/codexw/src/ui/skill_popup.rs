// OWNER: bottom-pane (slash popup, @ mentions)
//! The `$` popup (spec C.3.4), ported from Codex's `skill_popup.rs`: one row per skill with the
//! name, a `[Skill]` tag and the description, single line, cut with an ellipsis. Wizard has no
//! plugins or apps, so every row is a skill.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::slash_popup::{MAX_POPUP_ROWS, ScrollState};
use super::status_indicator::truncate_with_ellipsis;
use crate::skills::Skill;
use crate::style::palette;
use crate::wrap::{line_width, width_of};

/// Names longer than this are cut with an ellipsis (Codex `MENTION_NAME_TRUNCATE_LEN`).
const NAME_TRUNCATE_LEN: usize = 28;
/// Columns left of the rows and the hint.
const INSET: u16 = 2;

#[derive(Debug)]
pub struct SkillPopup {
    skills: Vec<Skill>,
    query: String,
    state: ScrollState,
}

/// Row indices for `query`, best first: name matches before the rest, then score, then name.
fn filtered(skills: &[Skill], query: &str) -> Vec<(usize, Vec<usize>)> {
    let q = query.trim();
    let mut out: Vec<(usize, Vec<usize>, i32)> = Vec::new();
    for (i, s) in skills.iter().enumerate() {
        if q.is_empty() {
            out.push((i, Vec::new(), 0));
        } else if let Some(m) = tuikit::fuzzy::score(q, &s.name) {
            out.push((i, m.indices, -m.score));
        }
    }
    if !q.is_empty() {
        out.sort_by(|a, b| {
            a.2.cmp(&b.2)
                .then_with(|| skills[a.0].name.cmp(&skills[b.0].name))
        });
    }
    out.into_iter().map(|(i, ix, _)| (i, ix)).collect()
}

impl SkillPopup {
    pub fn new(skills: Vec<Skill>, query: &str) -> Self {
        let mut p = SkillPopup {
            skills,
            query: String::new(),
            state: ScrollState::default(),
        };
        p.set_query(query);
        p
    }

    pub fn set_query(&mut self, query: &str) {
        self.query = query.to_string();
        let n = self.len();
        self.state.clamp_selection(n);
        self.state.ensure_visible(n, MAX_POPUP_ROWS.min(n));
    }

    fn len(&self) -> usize {
        filtered(&self.skills, &self.query).len()
    }

    pub fn move_up(&mut self) {
        let n = self.len();
        self.state.move_up_wrap(n);
        self.state.ensure_visible(n, MAX_POPUP_ROWS.min(n));
    }

    pub fn move_down(&mut self) {
        let n = self.len();
        self.state.move_down_wrap(n);
        self.state.ensure_visible(n, MAX_POPUP_ROWS.min(n));
    }

    pub fn selected(&self) -> Option<&Skill> {
        let rows = filtered(&self.skills, &self.query);
        let (i, _) = rows.get(self.state.selected_idx?)?;
        self.skills.get(*i)
    }

    /// Rows (at least one) plus the blank row and the hint.
    pub fn required_height(&self, _width: u16) -> u16 {
        self.len().clamp(1, MAX_POPUP_ROWS) as u16 + 2
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let (list, hint) = if area.height > 2 {
            (
                Rect::new(area.x, area.y, area.width, area.height - 2),
                Some(Rect::new(area.x, area.y + area.height - 1, area.width, 1)),
            )
        } else {
            (area, None)
        };
        let inner = Rect::new(
            list.x + INSET.min(list.width),
            list.y,
            list.width.saturating_sub(INSET),
            list.height,
        );
        self.render_rows(inner, buf);
        if let Some(h) = hint {
            let d = Style::default().dim();
            let line = Line::from(vec![
                Span::raw("Press "),
                Span::styled("enter", d),
                Span::raw(" to insert or "),
                Span::styled("esc", d),
                Span::raw(" to close"),
            ]);
            let x = h.x + INSET.min(h.width);
            let r = Rect::new(x, h.y, h.width.saturating_sub(INSET), 1);
            tuikit::paint::put_line(buf, r.x, r.y, &line, r);
        }
    }

    fn render_rows(&self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let rows = filtered(&self.skills, &self.query);
        if rows.is_empty() {
            let l = Line::from(Span::styled("no matches", Style::default().dim().italic()));
            tuikit::paint::put_line(buf, area.x, area.y, &l, area);
            return;
        }
        let visible = MAX_POPUP_ROWS.min(rows.len()).min(area.height as usize);
        let mut start = self.state.scroll_top.min(rows.len() - 1);
        if let Some(sel) = self.state.selected_idx {
            if sel < start {
                start = sel;
            } else if sel > start + visible - 1 {
                start = sel + 1 - visible;
            }
        }
        // The description column: the widest visible name, at most 70% of the width.
        let names: Vec<String> = rows
            .iter()
            .map(|(i, _)| cut_name(&self.skills[*i].name))
            .collect();
        let widest = names
            .iter()
            .skip(start)
            .take(visible)
            .map(|n| width_of(n))
            .max()
            .unwrap_or(0);
        let max_auto = (area.width as usize - 1).min(((area.width as usize * 7) / 10).max(1));
        let desc_col = (widest + 2).min(max_auto.max(1));
        for (n, (i, indices)) in rows.iter().enumerate().skip(start).take(visible) {
            let y = area.y + (n - start) as u16;
            let skill = &self.skills[*i];
            let selected = Some(n) == self.state.selected_idx;
            let line = row_line(
                skill,
                &names[n],
                indices,
                desc_col,
                area.width as usize,
                selected,
            );
            tuikit::paint::put_line(buf, area.x, y, &line, Rect::new(area.x, y, area.width, 1));
        }
    }
}

fn cut_name(name: &str) -> String {
    if name.chars().count() <= NAME_TRUNCATE_LEN {
        return name.to_string();
    }
    let mut s: String = name.chars().take(NAME_TRUNCATE_LEN - 1).collect();
    s.push('…');
    s
}

fn row_line(
    skill: &Skill,
    name: &str,
    indices: &[usize],
    desc_col: usize,
    width: usize,
    selected: bool,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let name_limit = desc_col.saturating_sub(2);
    let mut used = 0;
    for (ci, ch) in name.chars().enumerate() {
        let w = width_of(&ch.to_string());
        if used + w > name_limit {
            break;
        }
        used += w;
        let style = if indices.contains(&ci) {
            Style::default().bold()
        } else {
            Style::default()
        };
        spans.push(Span::styled(ch.to_string(), style));
    }
    spans.push(Span::raw(" ".repeat(desc_col.saturating_sub(used))));
    let desc = if skill.description.is_empty() {
        "[Skill]".to_string()
    } else {
        format!("[Skill] {}", skill.description)
    };
    spans.push(Span::styled(desc, Style::default().dim()));
    let mut line = truncate_with_ellipsis(Line::from(spans), width);
    if selected {
        let accent = palette().accent();
        line.spans.iter_mut().for_each(|s| s.style = accent);
    }
    debug_assert!(line_width(&line) <= width);
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn skills() -> Vec<Skill> {
        [
            (
                "Image Gen",
                "Generate or edit images for websites, games, and more",
            ),
            (
                "OpenAI Docs",
                "OpenAI and Codex docs for models, skills, tasks, and setup",
            ),
            ("Plugin Creator", "Scaffold plugins and marketplace entries"),
            ("Review Agent", "Find actionable bugs in code changes"),
            ("Skill Creator", "Create or update a skill"),
            (
                "Skill Installer",
                "Install curated skills from openai/skills or other repos",
            ),
        ]
        .map(|(n, d)| Skill {
            name: n.into(),
            description: d.into(),
            path: PathBuf::from("x"),
        })
        .to_vec()
    }

    fn dump(p: &SkillPopup, w: u16) -> Vec<String> {
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
    fn matches_the_capture_at_120_columns() {
        // reference/codex/120x36/start-11-skills-popup
        let rows = dump(&SkillPopup::new(skills(), ""), 120);
        assert_eq!(rows.len(), 8);
        assert_eq!(
            rows[0],
            "  Image Gen        [Skill] Generate or edit images for websites, games, and more"
        );
        assert_eq!(
            rows[5],
            "  Skill Installer  [Skill] Install curated skills from openai/skills or other repos"
        );
        assert_eq!(rows[6], "");
        assert_eq!(rows[7], "  Press enter to insert or esc to close");
    }

    #[test]
    fn long_descriptions_are_cut_with_an_ellipsis() {
        // reference/codex/80x24/start-11-skills-popup
        let rows = dump(&SkillPopup::new(skills(), ""), 80);
        assert_eq!(
            rows[1],
            "  OpenAI Docs      [Skill] OpenAI and Codex docs for models, skills, tasks, and…"
        );
        let rows = dump(&SkillPopup::new(skills(), ""), 40);
        assert_eq!(rows[0], "  Image Gen        [Skill] Generate or …");
    }

    #[test]
    fn selected_row_is_accent_and_the_rest_dim() {
        let p = SkillPopup::new(skills(), "");
        let area = Rect::new(0, 0, 120, 8);
        let mut buf = Buffer::empty(area);
        p.render(area, &mut buf);
        let accent = palette().accent();
        assert_eq!(
            buf[(0, 0)].fg,
            ratatui::style::Color::Reset,
            "inset stays plain"
        );
        assert_eq!(buf[(2, 0)].fg, accent.fg.unwrap());
        assert_eq!(buf[(30, 0)].fg, accent.fg.unwrap());
        assert!(
            buf[(30, 1)]
                .modifier
                .contains(ratatui::style::Modifier::DIM)
        );
        assert!(
            !buf[(2, 1)].modifier.contains(ratatui::style::Modifier::DIM),
            "name is plain"
        );
    }

    #[test]
    fn filter_ranks_by_score_and_no_match_is_italic() {
        let mut p = SkillPopup::new(skills(), "skill");
        assert_eq!(p.selected().unwrap().name, "Skill Creator");
        p.set_query("zzzz");
        assert_eq!(dump(&p, 80)[0], "  no matches");
        assert!(p.selected().is_none());
    }

    #[test]
    fn moving_wraps() {
        let mut p = SkillPopup::new(skills(), "");
        p.move_up();
        assert_eq!(p.selected().unwrap().name, "Skill Installer");
        p.move_down();
        assert_eq!(p.selected().unwrap().name, "Image Gen");
    }
}
