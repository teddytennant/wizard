//! The goal loop's independent verifier.
//!
//! A goal loop that trusts the builder's own "done" is grading its own
//! homework: the model that just decided the work is finished is the last one
//! who should rule on it. After a builder reports the goal complete, a *fresh*
//! critic subagent — one that never saw the builder's reasoning or claims —
//! looks at the real artifact and returns a binary verdict. The loop acts on
//! the verdict, not on the say-so. That is the whole difference between "the
//! model said done" and done.
//!
//! Binary on purpose. A score out of ten drifts up over rounds until every
//! artifact is a nine; `OURS` / `BAR` / `PLATEAU` cannot. `BAR` carries the one
//! gap worth another round, and two `PLATEAU`s in a row mean the critic cannot
//! name a gap another round would close, which is the loop's cue to stop rather
//! than churn.
//!
//! [`crate::agent::Agent::critique_goal`] runs the critic; this module owns the
//! critic's definition and the parsing of its verdict, both of which are pure
//! and tested here.

use crate::agent::subagent::SubagentConfig;
use crate::config::StepBudget;
use std::path::Path;

/// How many steps the critic may take: enough to read the files and run a quick
/// look, not enough to wander. It is read-only, so it cannot do harm with them.
const CRITIC_MAX_STEPS: u32 = 20;

/// Longest gap text carried back to the builder. A critic that writes an essay
/// still hands the loop one actionable line, not a wall.
const MAX_GAP_CHARS: usize = 600;

/// A critic's binary judgement of whether the goal is met.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalVerdict {
    /// The artifact meets the goal (beats the bar, if one was fetched).
    Ours,
    /// It does not yet. The single biggest gap to close before judging again.
    Bar(String),
    /// The critic cannot name a gap another round would close.
    Plateau,
}

impl GoalVerdict {
    /// A one-line description for a notice or a mission note.
    pub fn summary(&self) -> String {
        match self {
            Self::Ours => "OURS — the critic signed off on the current artifact".to_string(),
            Self::Bar(gap) => format!("BAR — {gap}"),
            Self::Plateau => {
                "PLATEAU — the critic can name no gap another round would close".to_string()
            }
        }
    }
}

/// The critic's system prompt. Independence is the load-bearing part: it is
/// spawned with a fresh context and no history, so it physically cannot see the
/// builder's turn, and this prompt tells it not to reconstruct one.
const CRITIC_SYSTEM_PROMPT: &str = "\
You are an independent critic for a goal-driven agent. You have never seen the \
builder and you get none of its reasoning, reports, or claims — only the goal \
and the project on disk. Do not imagine what the builder intended; judge what \
is actually there.\n\
\n\
Inspect the real files, not a summary of them. If a quality bar has been \
fetched under `.gauntlet/bar/`, compare the artifact against it; otherwise \
judge the artifact against the goal's own success criteria. If the goal is \
code, its tests passing is part of meeting it.\n\
\n\
Return your verdict as the FIRST line of your reply, exactly one of:\n\
  OURS      — the artifact meets the goal (beats the bar, if there is one)\n\
  BAR       — it does not; on the next line, name the SINGLE biggest gap\n\
  PLATEAU   — you cannot name a gap another round would close\n\
\n\
No scores out of ten. No praise. No advice beyond the one gap. If you are \
unsure whether a round could close the distance, that is PLATEAU, not OURS.";

/// The critic subagent's definition. Read-only is enforced by the spawn
/// options, not here, so this stays a plain description; `tool_scope` is left
/// open so the read-only registry keeps every inspection tool the install has.
pub fn critic_config() -> SubagentConfig {
    SubagentConfig {
        name: "goal-critic".to_string(),
        description: "Independent critic that judges whether the standing goal is met and returns a binary OURS/BAR/PLATEAU verdict.".to_string(),
        system_prompt: CRITIC_SYSTEM_PROMPT.to_string(),
        tool_scope: None,
        max_steps: StepBudget::new(CRITIC_MAX_STEPS),
    }
}

/// The task handed to the critic: the goal and where to look. Deliberately
/// spare — the critic forms its own view from the files, not from a briefing.
pub fn critic_task(goal: &str, project_root: &Path) -> String {
    format!(
        "The standing goal is:\n\n{goal}\n\nThe project is at {root}. Inspect the real \
         artifact there and judge whether the goal is met. If `.gauntlet/bar/` exists, that \
         is the quality bar to beat. Return your verdict now.",
        root = project_root.display()
    )
}

/// Does `upper` begin with `token` as a whole word (end of string or a
/// non-alphabetic char after it)? Guards against `BARELY` reading as `BAR`.
fn starts_with_token(upper: &str, token: &str) -> bool {
    upper
        .strip_prefix(token)
        .is_some_and(|rest| rest.chars().next().is_none_or(|c| !c.is_ascii_alphabetic()))
}

/// Parse a critic's reply into a verdict. `None` when it named none — the
/// caller decides what an unclear critic means (the loop treats it as "judge
/// again", never as a pass).
pub fn parse_verdict(output: &str) -> Option<GoalVerdict> {
    let lines: Vec<&str> = output.lines().collect();
    for (idx, line) in lines.iter().enumerate() {
        // Ignore leading markdown / bullet punctuation the model may prepend.
        let stripped = line
            .trim()
            .trim_start_matches(|c: char| !c.is_ascii_alphabetic());
        let upper = stripped.to_ascii_uppercase();
        if starts_with_token(&upper, "OURS") {
            return Some(GoalVerdict::Ours);
        }
        if starts_with_token(&upper, "PLATEAU") {
            return Some(GoalVerdict::Plateau);
        }
        if starts_with_token(&upper, "BAR") {
            return Some(GoalVerdict::Bar(gap_after(&lines, idx, stripped)));
        }
    }
    None
}

/// The gap text for a `BAR`: whatever follows `BAR` on its own line, then the
/// following lines, joined and trimmed to one carryable line.
fn gap_after(lines: &[&str], idx: usize, stripped_line: &str) -> String {
    let mut gap = String::new();
    // Remainder of the BAR line itself, after the token and any separator.
    let same_line = stripped_line[3..]
        .trim_start_matches([':', '-', '.', ' ', '\t'])
        .trim();
    if !same_line.is_empty() {
        gap.push_str(same_line);
    }
    for line in &lines[idx + 1..] {
        let piece = line.trim();
        if piece.is_empty() {
            if gap.is_empty() {
                continue;
            }
            break; // first blank line after some gap text ends the paragraph
        }
        if !gap.is_empty() {
            gap.push(' ');
        }
        gap.push_str(piece);
    }
    if gap.is_empty() {
        gap.push_str(
            "the critic returned BAR without naming a gap; identify the biggest gap and close it",
        );
    }
    brief(&gap, MAX_GAP_CHARS)
}

/// Trim to `max` chars on a char boundary, so a multi-byte gap cannot panic.
fn brief(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

/// What the loop should do next, given a verdict and how many `PLATEAU`s have
/// come in a row. Pure so the loop's control flow can be tested without a model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CriticAction {
    /// The goal is met: let the cycle land.
    Accept,
    /// Not yet: send this prompt back to the builder for another round.
    Rework(String),
    /// Plateaued twice: stop the loop and report `reason`.
    Stop(String),
}

/// Plateaus in a row that mean "stop", not "try once more".
pub const PLATEAU_LIMIT: u32 = 2;

/// Decide the loop's next move from a verdict. `plateau_streak` is the count
/// *including* this verdict when it is `Plateau` (the caller bumps it first).
pub fn plan_after_verdict(verdict: &GoalVerdict, plateau_streak: u32) -> CriticAction {
    match verdict {
        GoalVerdict::Ours => CriticAction::Accept,
        GoalVerdict::Bar(gap) => CriticAction::Rework(format!(
            "An independent critic judged the goal NOT yet met and named one gap to close:\n\n\
             {gap}\n\n\
             Close exactly this gap, then stop and let the critic judge again. Do not declare \
             the goal done yourself — the critic decides."
        )),
        GoalVerdict::Plateau if plateau_streak >= PLATEAU_LIMIT => CriticAction::Stop(
            "the critic returned PLATEAU twice: it can name no gap another round would close"
                .to_string(),
        ),
        GoalVerdict::Plateau => CriticAction::Rework(
            "An independent critic returned PLATEAU: it could not name a gap another round \
             would close. Take a genuinely different approach — a different angle, tool, or \
             decomposition — then let the critic judge again. If there is truly nothing more \
             to try, say so plainly."
                .to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_bare_verdict() {
        assert_eq!(parse_verdict("OURS"), Some(GoalVerdict::Ours));
        assert_eq!(parse_verdict("PLATEAU"), Some(GoalVerdict::Plateau));
    }

    #[test]
    fn parses_ours_with_trailing_prose() {
        assert_eq!(
            parse_verdict("OURS\nThe tests pass and it matches the bar."),
            Some(GoalVerdict::Ours)
        );
    }

    #[test]
    fn bar_captures_the_gap_on_the_next_line() {
        assert_eq!(
            parse_verdict("BAR\nThe retry path has no backoff, so a flapping host is hammered."),
            Some(GoalVerdict::Bar(
                "The retry path has no backoff, so a flapping host is hammered.".to_string()
            ))
        );
    }

    #[test]
    fn bar_captures_the_gap_on_the_same_line() {
        assert_eq!(
            parse_verdict("BAR: missing the error path for a closed socket"),
            Some(GoalVerdict::Bar(
                "missing the error path for a closed socket".to_string()
            ))
        );
    }

    #[test]
    fn ignores_leading_markdown() {
        assert_eq!(parse_verdict("**OURS**"), Some(GoalVerdict::Ours));
        assert_eq!(parse_verdict("- PLATEAU"), Some(GoalVerdict::Plateau));
    }

    #[test]
    fn does_not_read_barely_as_bar() {
        // No verdict token present at all.
        assert_eq!(parse_verdict("Barely acceptable, but fine."), None);
    }

    #[test]
    fn unclear_reply_has_no_verdict() {
        assert_eq!(parse_verdict("I think it looks pretty good overall."), None);
    }

    #[test]
    fn bar_without_a_gap_still_names_one() {
        let GoalVerdict::Bar(gap) = parse_verdict("BAR").unwrap() else {
            panic!("expected BAR");
        };
        assert!(!gap.is_empty());
    }

    #[test]
    fn a_long_gap_is_trimmed() {
        let huge = format!("BAR\n{}", "x".repeat(MAX_GAP_CHARS + 200));
        let GoalVerdict::Bar(gap) = parse_verdict(&huge).unwrap() else {
            panic!("expected BAR");
        };
        assert!(gap.chars().count() <= MAX_GAP_CHARS + 1); // +1 for the ellipsis
    }

    #[test]
    fn ours_accepts() {
        assert_eq!(
            plan_after_verdict(&GoalVerdict::Ours, 0),
            CriticAction::Accept
        );
    }

    #[test]
    fn bar_reworks_with_the_gap_in_the_prompt() {
        let CriticAction::Rework(prompt) =
            plan_after_verdict(&GoalVerdict::Bar("no backoff".into()), 0)
        else {
            panic!("expected rework");
        };
        assert!(prompt.contains("no backoff"));
        assert!(prompt.contains("critic decides"));
    }

    #[test]
    fn first_plateau_reworks_second_stops() {
        assert!(matches!(
            plan_after_verdict(&GoalVerdict::Plateau, 1),
            CriticAction::Rework(_)
        ));
        assert!(matches!(
            plan_after_verdict(&GoalVerdict::Plateau, PLATEAU_LIMIT),
            CriticAction::Stop(_)
        ));
    }
}
