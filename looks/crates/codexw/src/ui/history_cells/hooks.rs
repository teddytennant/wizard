//! Hook cells (spec B.9): what a lifecycle hook did, in Codex's wording. Wizard runs hooks from
//! `hooks.toml` and reports each as `hook <event>: <outcome> (<command>)`; ACP carries no hook
//! events, so a cell only appears when that text reaches the chat as a notice. Quiet outcomes
//! (rewrote arguments, appended context) leave no trace, as a Codex hook that completes with no
//! output does not.

use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};

use crate::ui::HistoryCell;
use crate::wrap::{WrapOpts, word_wrap_line};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookStatus {
    Completed,
    Blocked,
    Failed,
    Stopped,
}

impl HookStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Warning,
    Stop,
    Feedback,
    Context,
    Error,
}

impl EntryKind {
    fn prefix(self) -> &'static str {
        match self {
            Self::Warning => "warning: ",
            Self::Stop => "stop: ",
            Self::Feedback => "feedback: ",
            Self::Context => "hook context: ",
            Self::Error => "error: ",
        }
    }
}

#[derive(Clone, Debug)]
pub struct HookCell {
    /// `PreToolUse` and friends.
    pub event: String,
    pub status: HookStatus,
    pub entries: Vec<(EntryKind, String)>,
}

/// Context entries show this many rows in the viewport; more become `… +N lines`.
const CONTEXT_PREVIEW_ROWS: usize = 3;

/// Wizard's event id as Codex names it.
pub fn event_label(id: &str) -> Option<&'static str> {
    Some(match id {
        "pre_tool_use" => "PreToolUse",
        "post_tool_use" => "PostToolUse",
        "user_prompt_submit" => "UserPromptSubmit",
        "session_start" => "SessionStart",
        "session_end" => "SessionEnd",
        "stop" => "Stop",
        "pre_compact" => "PreCompact",
        "post_compact" => "PostCompact",
        "subagent_start" => "SubagentStart",
        "subagent_stop" => "SubagentStop",
        "permission_request" => "PermissionRequest",
        _ => return None,
    })
}

impl HookCell {
    /// The cell for wizard's `hook <event>: <outcome> (<command>)` notice, or `None` when the
    /// text is something else or the outcome is a quiet one.
    pub fn from_notice(text: &str) -> Option<HookCell> {
        let rest = text.trim().strip_prefix("hook ")?;
        let (event_id, rest) = rest.split_once(": ")?;
        let event = event_label(event_id)?;
        // The command is the last parenthesised group; the outcome's own text may contain some.
        let body = rest.rsplit_once(" (").map_or(rest, |(b, _)| b);
        if let Some(reason) = body.strip_prefix("blocked \u{2014} ") {
            let (status, kind) = if event == "UserPromptSubmit" {
                (HookStatus::Stopped, EntryKind::Stop)
            } else {
                (HookStatus::Blocked, EntryKind::Feedback)
            };
            return Some(HookCell {
                event: event.into(),
                status,
                entries: vec![(kind, reason.trim().to_string())],
            });
        }
        if let Some(why) = body.strip_prefix("warning \u{2014} ") {
            return Some(HookCell {
                event: event.into(),
                status: HookStatus::Failed,
                entries: vec![(EntryKind::Error, why.trim().to_string())],
            });
        }
        None
    }

    fn bullet(&self, has_warning: bool) -> Span<'static> {
        match self.status {
            HookStatus::Completed if !has_warning => {
                Span::styled("\u{2022}", Style::default().fg(Color::Green).bold())
            }
            HookStatus::Completed => Span::styled("\u{2022}", Style::default().bold()),
            _ => Span::styled("\u{2022}", Style::default().fg(Color::Red).bold()),
        }
    }

    fn lines(&self, width: u16, full: bool) -> Vec<Line<'static>> {
        let wrap = |text: &str, first: &str, rest: &str| -> Vec<Line<'static>> {
            let line = Line::from(format!("{first}{text}"));
            let opts = WrapOpts::new((width as usize).max(1))
                .subsequent_indent(Line::from(rest.to_string()));
            word_wrap_line(&line, &opts)
        };
        let warnings: Vec<&String> = self
            .entries
            .iter()
            .filter(|(k, _)| *k == EntryKind::Warning)
            .map(|(_, t)| t)
            .collect();
        let has_warning = !warnings.is_empty();
        let mut out: Vec<Line<'static>> = Vec::new();
        let status = self.status.label();
        if let Some(w) = warnings.first() {
            let mut it = w.split('\n');
            let first = it.next().unwrap_or("");
            out.push(Line::from(vec![
                self.bullet(true),
                Span::from(format!(" {} ({status}) says: {first}", self.event)),
            ]));
            for l in it {
                out.push(Line::from(if l.is_empty() {
                    String::new()
                } else {
                    format!("    {l}")
                }));
            }
        } else {
            out.push(Line::from(vec![
                self.bullet(has_warning),
                Span::from(format!(" {} hook ({status})", self.event)),
            ]));
        }
        for (kind, text) in &self.entries {
            if *kind == EntryKind::Warning {
                continue;
            }
            let rows: Vec<Line<'static>> = text
                .split('\n')
                .enumerate()
                .flat_map(|(i, l)| {
                    if i == 0 {
                        wrap(l, &format!("  {}", kind.prefix()), "    ")
                    } else if l.is_empty() {
                        vec![Line::default()]
                    } else {
                        wrap(l, "    ", "    ")
                    }
                })
                .collect();
            if *kind == EntryKind::Context && !full && rows.len() > CONTEXT_PREVIEW_ROWS {
                out.extend(rows.iter().take(2).cloned());
                out.push(Line::from(
                    format!(
                        "    \u{2026} +{} lines (ctrl + t to view transcript)",
                        rows.len() - 2
                    )
                    .dim(),
                ));
            } else {
                out.extend(rows);
            }
        }
        out
    }
}

impl HistoryCell for HookCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.lines(width, false)
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.lines(width, true)
    }
}

/// `• Running PreToolUse hook: checking command policy`, the line while a hook is out.
pub fn running_line(event: &str, count: usize, status_message: Option<&str>) -> Line<'static> {
    let what = if count > 1 {
        format!("Running {count} {event} hooks")
    } else {
        format!("Running {event} hook")
    };
    let mut spans = vec![
        Span::from("\u{2022} ").dim(),
        Span::styled(what, Style::default().bold()),
    ];
    if let Some(m) = status_message.filter(|m| !m.is_empty()) {
        spans.push(Span::from(format!(": {m}")).dim());
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn cell(event: &str, status: HookStatus, entries: &[(EntryKind, &str)]) -> HookCell {
        HookCell {
            event: event.into(),
            status,
            entries: entries.iter().map(|(k, t)| (*k, t.to_string())).collect(),
        }
    }

    // The five snapshots of the spec (chatwidget/snapshots).

    #[test]
    fn blocked_and_failed_hooks_show_their_entry() {
        let c = cell(
            "PreToolUse",
            HookStatus::Blocked,
            &[(EntryKind::Feedback, "run tests before touching the fixture")],
        );
        assert_eq!(
            text(&c.display_lines(80)),
            [
                "• PreToolUse hook (blocked)",
                "  feedback: run tests before touching the fixture"
            ]
        );
        let c = cell(
            "PostToolUse",
            HookStatus::Failed,
            &[(EntryKind::Error, "hook exited with code 7")],
        );
        assert_eq!(
            text(&c.display_lines(80)),
            [
                "• PostToolUse hook (failed)",
                "  error: hook exited with code 7"
            ]
        );
    }

    #[test]
    fn context_continuation_lines_indent_four() {
        let c = cell(
            "SessionStart",
            HookStatus::Completed,
            &[(EntryKind::Context, "session context\nsecond line")],
        );
        assert_eq!(
            text(&c.display_lines(80)),
            [
                "• SessionStart hook (completed)",
                "  hook context: session context",
                "    second line"
            ]
        );
    }

    #[test]
    fn a_warning_moves_up_to_the_header() {
        let c = cell(
            "PostToolUse",
            HookStatus::Completed,
            &[
                (EntryKind::Warning, "Heads up from the hook"),
                (EntryKind::Context, "Remember the startup checklist."),
            ],
        );
        assert_eq!(
            text(&c.display_lines(80)),
            [
                "• PostToolUse (completed) says: Heads up from the hook",
                "  hook context: Remember the startup checklist."
            ]
        );
        let c = cell(
            "UserPromptSubmit",
            HookStatus::Stopped,
            &[
                (EntryKind::Warning, "go-workflow must start from PlanMode"),
                (EntryKind::Stop, "prompt blocked"),
            ],
        );
        assert_eq!(
            text(&c.display_lines(80)),
            [
                "• UserPromptSubmit (stopped) says: go-workflow must start from PlanMode",
                "  stop: prompt blocked"
            ]
        );
    }

    #[test]
    fn the_running_line_collapses_identical_hooks() {
        assert_eq!(
            text(&[running_line(
                "PostToolUse",
                1,
                Some("checking output policy")
            )]),
            ["• Running PostToolUse hook: checking output policy"]
        );
        assert_eq!(
            text(&[running_line(
                "PreToolUse",
                3,
                Some("checking command policy")
            )]),
            ["• Running 3 PreToolUse hooks: checking command policy"]
        );
    }

    #[test]
    fn bullets_follow_the_status() {
        let b = |s, w| {
            cell(
                "Stop",
                s,
                &if w {
                    vec![(EntryKind::Warning, "w")]
                } else {
                    vec![]
                },
            )
            .display_lines(80)[0]
                .spans[0]
                .style
        };
        assert_eq!(b(HookStatus::Completed, false).fg, Some(Color::Green));
        assert_eq!(b(HookStatus::Completed, true).fg, None);
        assert_eq!(b(HookStatus::Blocked, false).fg, Some(Color::Red));
        assert_eq!(b(HookStatus::Stopped, false).fg, Some(Color::Red));
    }

    #[test]
    fn long_context_is_cut_in_the_viewport_and_whole_in_the_transcript() {
        let ctx = (1..=6)
            .map(|i| format!("context line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let c = cell(
            "SessionStart",
            HookStatus::Completed,
            &[(EntryKind::Context, &ctx)],
        );
        let short = text(&c.display_lines(80));
        assert_eq!(short.len(), 1 + 2 + 1);
        assert_eq!(
            short[3],
            "    \u{2026} +4 lines (ctrl + t to view transcript)"
        );
        assert_eq!(text(&c.transcript_lines(80)).len(), 1 + 6);
    }

    #[test]
    fn wizards_notice_becomes_a_cell_and_quiet_outcomes_do_not() {
        let c =
            HookCell::from_notice("hook pre_tool_use: blocked \u{2014} no rm -rf (/opt/policy.sh)")
                .unwrap();
        assert_eq!(
            text(&c.display_lines(80)),
            ["• PreToolUse hook (blocked)", "  feedback: no rm -rf"]
        );
        let c =
            HookCell::from_notice("hook user_prompt_submit: blocked \u{2014} not today (check.sh)")
                .unwrap();
        assert_eq!(
            text(&c.display_lines(80)),
            ["• UserPromptSubmit hook (stopped)", "  stop: not today"]
        );
        let c = HookCell::from_notice(
            "hook post_tool_use: warning \u{2014} timed out after 60s (fmt.sh)",
        )
        .unwrap();
        assert_eq!(
            text(&c.display_lines(80)),
            [
                "• PostToolUse hook (failed)",
                "  error: timed out after 60s"
            ]
        );
        assert!(HookCell::from_notice("hook pre_tool_use: updated args (x.sh)").is_none());
        assert!(HookCell::from_notice("hook pre_tool_use: appended context (x.sh)").is_none());
        assert!(HookCell::from_notice("hook nonsense: blocked \u{2014} x (y)").is_none());
        assert!(HookCell::from_notice("hooks are great").is_none());
    }
}
