//! The lines at the top of the transcript: the product, the model, the version, the providers
//! that have a model on offer, and the working directory. They scroll away with the transcript.

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

use super::line::{Hang, RLine};
use crate::app::App;
use crate::theme;

fn title_word(w: &str) -> String {
    let letters = w.chars().all(|c| c.is_ascii_alphabetic());
    let vowelless = letters && !w.chars().any(|c| "aeiouyAEIOUY".contains(c));
    if vowelless && w.len() <= 4 {
        return w.to_uppercase();
    }
    let mut c = w.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// `grok-4.7` as `Grok 4.7`, `gpt-5.1-codex-max` as `GPT-5.1 Codex Max`,
/// `claude-opus-4-6` as `Claude 4.6 Opus`.
pub fn model_display_name(id: &str) -> String {
    if let Some((provider, rest)) = id.split_once('/') {
        return format!("{provider}: {rest}");
    }
    let parts: Vec<&str> = id
        .split('-')
        .filter(|p| !(p.len() >= 6 && p.chars().all(|c| c.is_ascii_digit())))
        .collect();
    match parts.as_slice() {
        ["claude", family, rest @ ..] if !rest.is_empty() => {
            format!("Claude {} {}", rest.join("."), title_word(family))
        }
        ["gpt", version, rest @ ..] => {
            let mut s = format!("GPT-{version}");
            for r in rest {
                s.push(' ');
                s.push_str(&title_word(r));
            }
            s
        }
        _ => parts
            .iter()
            .map(|p| title_word(p))
            .collect::<Vec<_>>()
            .join(" "),
    }
}

pub fn home_relative(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && path == h => "~".into(),
        Ok(h) if !h.is_empty() && path.starts_with(&format!("{h}/")) => {
            format!("~{}", &path[h.len()..])
        }
        _ => path.to_string(),
    }
}

pub fn lines(app: &App, width: usize) -> Vec<RLine> {
    let dim = Style::default().fg(theme::dim());
    let mut out = vec![RLine::new(vec![Span::styled(
        "wizard",
        Style::default()
            .fg(theme::header_name())
            .add_modifier(Modifier::BOLD),
    )])];
    let model = app.model_name();
    if !model.is_empty() {
        let nice = model_display_name(&model);
        let fit = width.saturating_sub(4);
        let mut spans = Vec::new();
        let mut used = nice.width();
        let hint = "/model to switch · ";
        if used + hint.width() <= fit {
            spans.push(Span::styled(hint, dim));
            used += hint.width();
        }
        let provider = app.provider();
        if !provider.is_empty() {
            let p = format!("{provider} · ");
            if used + p.width() <= fit {
                spans.push(Span::styled(p, dim));
            }
        }
        spans.push(Span::styled(
            nice,
            Style::default()
                .fg(theme::model_pink())
                .add_modifier(Modifier::BOLD),
        ));
        out.push(RLine::new(spans).hang(Hang::None));
    }
    out.push(RLine::styled(format!("v{}", app.config.version()), dim));
    let mut seen: Vec<&str> = Vec::new();
    for m in &app.config.models {
        if !m.provider.is_empty() && !seen.contains(&m.provider.as_str()) {
            seen.push(&m.provider);
        }
    }
    for p in seen {
        out.push(RLine::new(vec![
            Span::styled("●", Style::default().fg(theme::success())),
            Span::styled(format!(" {p}"), dim),
        ]));
    }
    let cwd = home_relative(&app.cwd());
    if !cwd.is_empty() {
        let with_branch = match &app.git.branch {
            Some(b) => format!("{cwd} ({b})"),
            None => cwd.clone(),
        };
        let text = if with_branch.width() <= width {
            with_branch
        } else {
            cwd
        };
        out.push(RLine::styled(text, dim).hang(Hang::None));
    }
    out.push(RLine::blank());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_names() {
        assert_eq!(model_display_name("grok-4.7"), "Grok 4.7");
        assert_eq!(model_display_name("grok-4.7-fast"), "Grok 4.7 Fast");
        assert_eq!(model_display_name("gpt-5.1-codex-max"), "GPT-5.1 Codex Max");
        assert_eq!(model_display_name("claude-opus-4-6"), "Claude 4.6 Opus");
        assert_eq!(
            model_display_name("claude-sonnet-5-20260101"),
            "Claude 5 Sonnet"
        );
    }
}
