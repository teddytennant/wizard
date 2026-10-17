// OWNER: footer (cwd and branch row, usage and model row, extension status)
//! Two rows, all `dim`: `<cwd> (<branch>) • <name>`, then usage stats left and
//! `[(provider) ]model • thinking level` right. Spec 8.

use ratatui::text::{Line, Span};
use tuikit::width::{clip_spans, display_width, spans_width};

use super::{home_path, span, truncate_dots, Cx, Lines};
use crate::app::App;
use crate::theme::Tok;

/// Pi's `formatTokens`.
pub fn format_tokens(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else if n < 10_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else if n < 1_000_000 {
        format!("{}k", (n as f64 / 1000.0).round() as u64)
    } else if n < 10_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else {
        format!("{}M", (n as f64 / 1_000_000.0).round() as u64)
    }
}

pub fn render(app: &App, cx: &Cx) -> Lines {
    let th = cx.th();
    let dim = th.fg(Tok::Dim);
    let w = cx.width as usize;

    // row 1
    let mut cwd = home_path(&app.cwd, &cx.home);
    if let Some(b) = &app.branch {
        cwd.push_str(&format!(" ({b})"));
    }
    if let Some(n) = &app.session_name {
        cwd.push_str(&format!(" • {n}"));
    }
    let row1 = truncate_dots(Line::from(span(cwd, dim)), w, dim);

    // row 2, left: stats
    let u = &app.transcript.usage;
    let mut parts: Vec<String> = Vec::new();
    if u.input_tokens > 0 {
        parts.push(format!("↑{}", format_tokens(u.input_tokens)));
    }
    if u.output_tokens > 0 {
        parts.push(format!("↓{}", format_tokens(u.output_tokens)));
    }
    if u.cached_tokens > 0 {
        parts.push(format!("R{}", format_tokens(u.cached_tokens)));
    }
    if let Some(c) = u.cost_usd.filter(|c| *c > 0.0) {
        parts.push(format!("${c:.3}"));
    }
    let known_window = u.context_window > 0;
    let pct = if known_window {
        Some(u.context_tokens as f64 * 100.0 / u.context_window as f64)
    } else {
        None
    };
    let ctx = match pct {
        Some(p) if !app.context_unknown => {
            format!("{p:.1}%/{} (auto)", format_tokens(u.context_window))
        }
        Some(_) => format!("?/{} (auto)", format_tokens(u.context_window)),
        None => "?/?".to_string(),
    };
    let ctx_tok = match pct {
        Some(p) if p > 90.0 => Tok::Error,
        Some(p) if p > 70.0 => Tok::Warning,
        _ => Tok::Dim,
    };
    let mut left: Vec<Span<'static>> = Vec::new();
    if !parts.is_empty() {
        left.push(span(format!("{} ", parts.join(" ")), dim));
    }
    left.push(span(ctx, th.fg(ctx_tok)));

    // a stats string wider than the row is cut with `...`; truncateToWidth(text, w, "...")
    if spans_width(&left) > w {
        left = if w <= 3 {
            vec![span(".".repeat(w), dim)]
        } else {
            let mut v = clip_spans(left, w - 3);
            v.push(span("...", dim));
            v
        };
    }
    let lw = spans_width(&left);

    // row 2, right: model and thinking level. Pi drops the provider when it would not leave two
    // columns after the stats, and cuts the right side (no ellipsis) before it touches the stats.
    let right_text = right_side(app, lw, w);
    let rw = display_width(&right_text);
    let row2 = if lw + 2 + rw <= w {
        let mut v = left;
        v.push(span(" ".repeat(w - lw - rw), dim));
        v.push(span(right_text, dim));
        Line::from(v)
    } else if w > lw + 2 {
        let cut = super::cut(&right_text, w - lw - 2);
        let cw = display_width(&cut);
        let mut v = left;
        v.push(span(" ".repeat(w - lw - cw), dim));
        v.push(span(cut, dim));
        Line::from(v)
    } else {
        Line::from(left)
    };
    vec![row1, row2]
}

fn right_side(app: &App, left_width: usize, w: usize) -> String {
    let cfg = &app.config;
    let opt = cfg.models.iter().find(|m| m.id == cfg.model);
    let id = opt
        .map(|m| m.name.clone())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| cfg.model.clone());
    let mut providers: Vec<&str> = cfg.models.iter().map(|m| m.provider.as_str()).collect();
    providers.sort();
    providers.dedup();
    let level = (!cfg.efforts.is_empty()).then(|| app.thinking_level());
    let mut s = id;
    if let Some(l) = level {
        s.push_str(&format!(" • {l}"));
    }
    if providers.len() > 1 {
        if let Some(m) = opt.filter(|m| !m.provider.is_empty()) {
            let with = format!("({}) {s}", m.provider);
            if left_width + 2 + display_width(&with) <= w {
                return with;
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_format_matches_pi() {
        assert_eq!(format_tokens(485), "485");
        assert_eq!(format_tokens(2100), "2.1k");
        assert_eq!(format_tokens(15_000), "15k");
        assert_eq!(format_tokens(200_000), "200k");
        assert_eq!(format_tokens(1_000_000), "1.0M");
    }
}
