// OWNER: visual
//! The one status line: mode, model and effort, cwd and branch on the left; context gauge,
//! cost and the palette hint on the right. It never wraps and drops segments in a fixed order.

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use tuikit::width::{display_width, spans_width, truncate_line};

use crate::palette::{Glyphs, Palette};

#[derive(Clone, Debug, Default)]
pub struct StatusInfo {
    pub mode: String,
    pub model: String,
    pub effort: String,
    pub cwd: String,
    pub branch: String,
    pub dirty: bool,
    pub ctx_tokens: u64,
    pub ctx_window: u64,
    pub cost: Option<f64>,
    pub nav: bool,
    /// The backend died or never started; the row says so instead of "starting".
    pub down: bool,
    pub busy: bool,
    pub spinner: &'static str,
    pub detail: bool,
    /// `(current, total)` while a search is active.
    pub search: Option<(usize, usize)>,
    /// Transient message that replaces the right group, e.g. `copied 214 chars`.
    pub flash: Option<(String, bool)>,
}

/// The design's names for the backend's permission modes.
pub fn mode_word(raw: &str) -> &str {
    match raw {
        "default" | "manual" => "ask",
        "acceptEdits" => "edits",
        "plan" => "plan",
        "bypassPermissions" => "bypass",
        "dontAsk" => "dontask",
        "auto" => "auto",
        other => other,
    }
}

fn mode_style(raw: &str, p: &Palette) -> Style {
    match raw {
        "default" | "manual" => p.s_dim(),
        "acceptEdits" => Style::new().fg(p.info),
        "plan" => p.s_accent(),
        "bypassPermissions" => p.s_err().add_modifier(Modifier::BOLD),
        _ => p.s_dim(),
    }
}

pub fn ctx_percent(tokens: u64, window: u64) -> Option<u32> {
    (window > 0).then(|| ((tokens as f64 / window as f64) * 100.0).round().min(999.0) as u32)
}

/// Filled and empty cells of the 8 cell gauge. At 6 percent one cell is filled, never zero, so
/// a started context does not read as an empty one.
pub fn gauge(pct: u32, g: &Glyphs) -> (String, String) {
    let on = ((pct.min(100) as f64 * 8.0 / 100.0).ceil() as usize).min(8);
    (g.gauge_on.repeat(on), g.gauge_off.repeat(8 - on))
}

/// Colour of the filled cells and the percent: warm from 70, red from 90.
pub fn gauge_style(pct: u32, p: &Palette) -> Style {
    if pct >= 90 {
        p.s_err()
    } else if pct >= 70 {
        p.s_warn()
    } else {
        p.s_accent()
    }
}

/// Cells before the mode word, the same whether or not a turn runs or nav mode is on, so the
/// word the eye anchors on never moves. From 80 columns the slot holds `NAV ` (4) and the
/// spinner (2, next to the word); below that the two share 4 and `NAV` wins, which is what keeps the cost on a
/// 60 column row.
pub fn slot_width(width: usize) -> usize {
    if width >= 80 {
        6
    } else {
        4
    }
}

fn slot(info: &StatusInfo, width: usize, p: &Palette) -> Vec<Span<'static>> {
    let total = slot_width(width);
    let nav = Span::styled("NAV ", p.s_accent().add_modifier(Modifier::BOLD));
    let mut out = Vec::new();
    // The spinner is the last two cells of the slot, next to the word it belongs to (round 2
    // finding 12: it used to sit at the far left, 6 to 10 cells from the mode).
    if total == 6 {
        out.push(if info.nav { nav } else { Span::raw("    ") });
        if info.busy {
            out.push(Span::styled(format!("{} ", info.spinner), p.s_accent()));
        } else {
            out.push(Span::raw("  "));
        }
    } else if info.nav {
        out.push(nav);
    } else if info.busy {
        out.push(Span::styled(format!("  {} ", info.spinner), p.s_accent()));
    } else {
        out.push(Span::raw("    "));
    }
    out
}

/// Shorten a path for the status line: `~` for home, middle cut when long.
pub fn short_path(path: &str, max: usize) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let p = if !home.is_empty() && path.starts_with(&home) {
        format!("~{}", &path[home.len()..])
    } else {
        path.to_string()
    };
    if display_width(&p) <= max {
        return p;
    }
    tuikit::width::truncate_left(&p, max)
}

struct Parts {
    hint: bool,
    cwd: bool,
    cost: bool,
    effort: bool,
    gauge: bool,
    branch: bool,
    percent: bool,
}

fn build(
    info: &StatusInfo,
    parts: &Parts,
    width: usize,
    p: &Palette,
    g: &Glyphs,
) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
    let sep = || Span::styled(format!(" {} ", g.sep), p.s_faint());
    let mut left: Vec<Span<'static>> = slot(info, width, p);
    left.push(Span::styled(
        mode_word(&info.mode).to_string(),
        mode_style(&info.mode, p),
    ));
    left.push(sep());
    left.push(Span::styled(info.model.clone(), p.s_text()));
    if parts.effort && !info.effort.is_empty() {
        left.push(Span::styled(format!(" {}", info.effort), p.s_dim()));
    }
    if parts.cwd && !info.cwd.is_empty() {
        left.push(sep());
        left.push(Span::styled(short_path(&info.cwd, 32), p.s_dim()));
    }
    if parts.branch && !info.branch.is_empty() {
        left.push(sep());
        left.push(Span::styled(info.branch.clone(), p.s_dim()));
        if info.dirty {
            left.push(Span::styled("*", p.s_warn()));
        }
    }
    let mut right: Vec<Span<'static>> = Vec::new();
    if let Some((msg, warn)) = &info.flash {
        right.push(Span::styled(
            msg.clone(),
            if *warn { p.s_warn() } else { p.s_accent() },
        ));
        return (left, right);
    }
    let pct = ctx_percent(info.ctx_tokens, info.ctx_window);
    let push_sep = |r: &mut Vec<Span<'static>>| {
        if !r.is_empty() {
            r.push(sep());
        }
    };
    if let Some((cur, total)) = info.search {
        if total == 0 {
            right.push(Span::styled("no match", p.s_warn()));
        } else {
            right.push(Span::styled(format!("match {cur}/{total}"), p.s_accent()));
        }
    }
    if info.detail {
        push_sep(&mut right);
        right.push(Span::styled("detail", p.s_accent()));
    }
    if parts.percent {
        if let Some(pct) = pct {
            push_sep(&mut right);
            let st = if pct >= 90 {
                p.s_err()
            } else if pct >= 70 {
                p.s_warn()
            } else {
                p.s_dim()
            };
            right.push(Span::styled(format!("ctx {pct}%"), st));
            if parts.gauge {
                let gs = gauge_style(pct, p);
                let (on, off) = gauge(pct, g);
                right.push(Span::raw(" "));
                right.push(Span::styled(on, gs));
                right.push(Span::styled(off, p.s_faint()));
            }
        } else if info.ctx_tokens > 0 {
            push_sep(&mut right);
            right.push(Span::styled(
                format!("ctx {}", crate::ui::row::fmt_tokens(info.ctx_tokens)),
                p.s_dim(),
            ));
        }
    }
    if parts.cost {
        if let Some(c) = info.cost.filter(|c| *c > 0.0) {
            push_sep(&mut right);
            right.push(Span::styled(format!("${c:.2}"), p.s_dim()));
        }
    }
    if parts.hint {
        push_sep(&mut right);
        right.push(Span::styled("ctrl+p", p.s_faint()));
    }
    (left, right)
}

/// The status row as spans, exactly `width` cells wide, padded 2 columns at both ends.
pub fn line(info: &StatusInfo, width: usize, p: &Palette, g: &Glyphs) -> Vec<Span<'static>> {
    let inner = width.saturating_sub(4);
    if info.mode.is_empty() && info.model.is_empty() {
        // Before the backend has said hello there is nothing to show but that.
        let msg = if info.down {
            "claude is not running; send a message to start it".to_string()
        } else {
            format!("starting claude{}", g.ellipsis)
        };
        let mut out = vec![Span::raw("  "), Span::styled(msg, p.s_faint())];
        let used = spans_width(&out);
        out.push(Span::raw(" ".repeat(width.saturating_sub(used))));
        return out;
    }
    let mut parts = Parts {
        hint: true,
        cwd: true,
        cost: true,
        effort: true,
        gauge: true,
        branch: true,
        percent: true,
    };
    // Drop order from the design: hint, cwd, cost, effort, gauge, then the branch.
    let drops: [fn(&mut Parts); 7] = [
        |p| p.hint = false,
        |p| p.cwd = false,
        |p| p.cost = false,
        |p| p.effort = false,
        |p| p.gauge = false,
        |p| p.branch = false,
        |p| p.percent = false,
    ];
    let mut step = 0;
    let (mut left, mut right) = build(info, &parts, width, p, g);
    let fits = |l: &[Span<'static>], r: &[Span<'static>]| {
        spans_width(l) + if r.is_empty() { 0 } else { 2 + spans_width(r) } <= inner
    };
    while !fits(&left, &right) && step < drops.len() {
        drops[step](&mut parts);
        step += 1;
        (left, right) = build(info, &parts, width, p, g);
    }
    let mut out = vec![Span::raw("  ")];
    if !fits(&left, &right) {
        // Mode and model never drop; cut the model name instead of wrapping.
        let l = truncate_line(&ratatui::text::Line::from(left), inner);
        out.extend(l.spans);
        let used = spans_width(&out);
        out.push(Span::raw(" ".repeat(width.saturating_sub(used))));
        return out;
    }
    let gap = inner - spans_width(&left) - spans_width(&right);
    out.extend(left);
    out.push(Span::raw(" ".repeat(gap)));
    out.extend(right);
    let used = spans_width(&out);
    out.push(Span::raw(" ".repeat(width.saturating_sub(used))));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::{Depth, Kind, UNICODE};

    fn info() -> StatusInfo {
        StatusInfo {
            mode: "default".into(),
            model: "sonnet-5.5".into(),
            effort: "high".into(),
            cwd: "/home/me/code/openw".into(),
            branch: "main".into(),
            dirty: true,
            ctx_tokens: 36_000,
            ctx_window: 200_000,
            cost: Some(0.31),
            ..StatusInfo::default()
        }
    }

    fn text(width: usize) -> String {
        let p = Palette::new(Kind::Hearth, Depth::True);
        line(&info(), width, &p, &UNICODE)
            .iter()
            .map(|s| s.content.as_ref())
            .collect()
    }

    #[test]
    fn is_one_row_of_exact_width_at_every_size() {
        for w in [30, 40, 60, 80, 120, 160, 200] {
            assert_eq!(display_width(&text(w)), w, "width {w}: {:?}", text(w));
        }
    }

    #[test]
    fn full_line_at_120() {
        let t = text(120);
        assert!(
            t.contains("ask · sonnet-5.5 high · /home/me/code/openw · main*"),
            "{t:?}"
        );
        assert!(t.contains("main*"));
        assert!(
            t.trim_end().ends_with("ctx 18% ━━────── · $0.31 · ctrl+p"),
            "{t:?}"
        );
    }

    #[test]
    fn degrades_in_the_stated_order() {
        let t = text(60);
        assert!(t.contains("ask · sonnet-5.5 high · main*"), "{t:?}");
        // The 4 cell slot costs the 60 column row its cost segment (round 1 kept it).
        assert!(t.trim_end().ends_with("ctx 18% ━━──────"), "{t:?}");
        let t = text(40);
        // At 40 columns the 4 cell slot takes the branch's place (round 1 kept it).
        assert!(t.contains("ask · sonnet-5.5"), "{t:?}");
        assert!(t.trim_end().ends_with("ctx 18%"), "{t:?}");
    }

    #[test]
    fn bypass_is_never_quiet_and_nav_is_shown() {
        let p = Palette::new(Kind::Hearth, Depth::True);
        let mut i = info();
        i.mode = "bypassPermissions".into();
        i.nav = true;
        let spans = line(&i, 100, &p, &UNICODE);
        let all: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(all.contains("NAV") && all.contains("bypass"));
        let by = spans.iter().find(|s| s.content == "bypass").unwrap();
        assert!(by.style.add_modifier.contains(Modifier::BOLD));
    }

    fn column_of(info: &StatusInfo, width: usize, word: &str) -> usize {
        let p = Palette::new(Kind::Hearth, Depth::True);
        let t: String = line(info, width, &p, &UNICODE)
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        let i = t.find(word).unwrap_or_else(|| panic!("{word} in {t:?}"));
        display_width(&t[..i])
    }

    #[test]
    fn the_mode_word_never_moves_when_a_turn_starts_or_nav_comes_on() {
        // Finding 5: three different columns for the same word in round 1.
        for w in [44, 60, 80, 120, 200] {
            let idle = column_of(&info(), w, "ask");
            let mut busy = info();
            busy.busy = true;
            busy.spinner = "⠋";
            let mut nav = info();
            nav.nav = true;
            let mut both = busy.clone();
            both.nav = true;
            for i in [&busy, &nav, &both] {
                assert_eq!(column_of(i, w, "ask"), idle, "width {w}");
            }
            assert_eq!(idle, 2 + slot_width(w), "width {w}");
            // The model name follows the mode word at a fixed offset too.
            assert_eq!(
                column_of(&both, w, "sonnet") - column_of(&both, w, "ask"),
                column_of(&info(), w, "sonnet") - idle
            );
        }
    }

    #[test]
    fn the_gauge_is_two_weights_of_one_rule_and_turns_warm_with_use() {
        let p = Palette::new(Kind::Hearth, Depth::True);
        let at = |tokens: u64| {
            let mut i = info();
            i.ctx_tokens = tokens;
            line(&i, 120, &p, &UNICODE)
        };
        let find = |spans: &[Span<'static>], c: &str| {
            spans
                .iter()
                .find(|s| s.content.starts_with(c))
                .map(|s| s.style.fg)
        };
        assert_eq!(find(&at(12_000), "━"), Some(Some(p.accent)));
        assert_eq!(find(&at(150_000), "━"), Some(Some(p.warn)));
        assert_eq!(find(&at(190_000), "━"), Some(Some(p.err)));
        // The empty part is `faint`, which clears 4.5:1 on `bg`, unlike `line`.
        assert_eq!(find(&at(12_000), "─"), Some(Some(p.faint)));
        assert!(crate::palette::contrast(p.faint, p.bg) >= 4.5);
    }
}
