//! Independent verification of a claim of done.
//!
//! An agent that trusts its own "done" is grading its own homework: the model
//! that just decided the work is finished is the last one who should rule on
//! it. Both verifiers here answer that by spawning a *fresh* subagent, one
//! that never saw the builder's reasoning or claims, and acting on its verdict
//! rather than on the say-so.
//!
//! Two of them, because they answer different questions.
//!
//! # The goal critic
//!
//! Mission scope, for `/goal`: *is the standing goal achieved?* Every time the
//! builder stops, whatever it said about the work, a harsh critic that can read
//! the tree and run commands but not write examines the project. Its verdict is
//! binary: `ACHIEVED` ends the loop, `NOT ACHIEVED` sends the critic's own
//! account of what has to change back to the builder. The builder's opinion of
//! its own work is not an input, so it cannot summon the critic or dodge it.
//! There is no third verdict that lets the loop give up. The loop stops when
//! the critic signs off, the user stops it, the turn itself fails, or a bound
//! trips: [`GOAL_IDLE_LIMIT`] turns that changed nothing, or
//! [`GOAL_CRITIC_FAILURE_LIMIT`] critics that could not run.
//! [`crate::agent::Agent::critique_goal`] runs it.
//!
//! # The completion review
//!
//! Turn scope, for every autonomous run: *is the request you just called done
//! actually satisfied?* One pass, never a loop of them, and one round of
//! feedback when it fails. It shares the goal critic's machinery (the fresh
//! subagent, the first-line verdict, the gap carried back) and differs in what
//! it is told to do: restate the request, check that everything the request
//! named exists and is not empty, re-run the acceptance path the request states,
//! and ask whether the answer survives a change of input. See [`review_config`].
//!
//! # Continuous cycles
//!
//! A continuous run's cycle ending is not a claim that the mission is done. A
//! mission can run for months, and the honest report at the end of a cycle is
//! "merged these, tests pass, this is what remains". Judging that against the
//! whole mission fails every cycle, and a builder told over and over that it
//! claimed a finish it never claimed learns to fake one. So both verifiers take
//! a [`Claim`]: [`Claim::Finished`] for a run meant to finish, judged as above,
//! and [`Claim::Cycle`] for a continuous cycle, where the verifier gets the
//! cycle's report and judges whether what it says is true.
//!
//! Both verdict parsers and both decision functions are pure and tested here.

use crate::agent::DoneReason;
use crate::agent::subagent::SubagentConfig;
use crate::config::StepBudget;
use crate::llm::{ChatMessage, Role};
use std::path::Path;
use std::time::Duration;

/// How many steps the goal critic may take. Enough to read the deliverables,
/// build, run the test suite and poke at an edge case or two. It cannot write,
/// so the steps can only be spent looking.
const CRITIC_MAX_STEPS: u32 = 40;

/// Longest gap text the completion review carries back.
const MAX_GAP_CHARS: usize = 600;

/// Longest critic feedback carried back to the builder. The goal critic is
/// asked for a list of what to change, not one line, so it gets more room.
const MAX_FEEDBACK_CHARS: usize = 4000;

/// Turns in a row that did no work before the loop decides the builder is
/// stuck and stops. A turn that wrote a file or ran a command resets it, so an
/// honest long grind never trips it, and one that only says it is finished
/// counts, so a builder arguing with the critic cannot run forever.
pub const GOAL_IDLE_LIMIT: u32 = 3;

/// Critics in a row that failed to reach a verdict before the loop stops. A
/// verdict nobody could produce tells the builder nothing, and an outage would
/// otherwise be billed a builder turn per attempt.
pub const GOAL_CRITIC_FAILURE_LIMIT: u32 = 3;

/// How long one goal critic may run. Long enough to sit through a slow build
/// and test suite. A hung critic otherwise holds the loop forever, with
/// nothing on screen to say so but `critic reviewing`.
pub const GOAL_CRITIC_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// The critic's verdict on whether the goal is achieved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalVerdict {
    /// The critic checked and signed off.
    Achieved,
    /// It is not achieved. The critic's feedback on what to do differently.
    NotAchieved(String),
}

impl GoalVerdict {
    /// A one-line description for a notice or a mission note.
    pub fn summary(&self) -> String {
        match self {
            Self::Achieved => "ACHIEVED: the critic signed off".to_string(),
            Self::NotAchieved(feedback) => format!("NOT ACHIEVED: {feedback}"),
        }
    }
}

/// The critic's system prompt. Independence is the load-bearing part: it is
/// spawned with a fresh context and no history, so it physically cannot see the
/// builder's turn, and this prompt tells it not to reconstruct one.
const CRITIC_SYSTEM_PROMPT: &str = "\
You are a harsh, independent critic. An agent working toward a goal has \
stopped, and every time it stops you examine the result. It may believe it is \
finished or it may have stalled partway. You never saw it work and you get \
none of its reasoning, summary or claims: only the goal and the project on \
disk. Your default position is that the goal is not achieved, and your job is \
to find out how.\n\
\n\
Check the real thing, not a description of it.\n\
1. Restate the goal and list every requirement it states or clearly implies.\n\
2. Check each one on disk. A missing file, a stub, a TODO, a placeholder, a \
hardcoded answer, a disabled or deleted test, or a feature that only handles \
the happy path is a failure.\n\
3. Build it and run its tests, and any command the goal says the result will \
be checked with, exactly as written. Code that does not build or tests that \
fail mean NOT ACHIEVED.\n\
4. Try at least one input or case the agent probably did not try.\n\
If `.gauntlet/bar/` exists, it is a quality bar the work must beat.\n\
\n\
You judge, you do not build. Do not write, edit or delete anything.\n\
\n\
The FIRST line of your reply is your verdict, exactly one of:\n\
  ACHIEVED       every requirement holds and you ran something that shows it\n\
  NOT ACHIEVED   anything less\n\
\n\
After NOT ACHIEVED, tell the agent what needs to change: each concrete problem \
you found, the evidence (a path, a command and its output), what has to change, \
and what is still missing. Most important first. No praise, no scores, no summary of what \
is fine. If you could not verify something, that is NOT ACHIEVED, and say \
what stopped you. When in doubt, it is NOT ACHIEVED.";

/// The critic subagent's definition. Write access is removed by the spawn
/// scope ([`crate::agent::subagent::RunScope::Inspect`]), not here, so the
/// critic keeps every inspection tool the install has plus `execute`.
pub fn critic_config() -> SubagentConfig {
    SubagentConfig {
        name: "goal-critic".to_string(),
        description: "Harsh independent critic that checks whether the standing goal is achieved \
                      and returns ACHIEVED or NOT ACHIEVED with what to change."
            .to_string(),
        system_prompt: CRITIC_SYSTEM_PROMPT.to_string(),
        tool_scope: None,
        max_steps: StepBudget::new(CRITIC_MAX_STEPS),
    }
}

/// The task handed to the critic: the goal and where to look. Deliberately
/// spare: the critic forms its own view from the files, not from a briefing.
pub fn critic_task(goal: &str, project_root: &Path) -> String {
    format!(
        "The goal, verbatim between the markers:\n\n<goal>\n{goal}\n</goal>\n\nThe agent has \
         stopped working on it. The project is at {root}. Check it and return your verdict \
         now.",
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

/// Parse the critic's reply. Only the first non-empty line is read for the
/// verdict, so "achieved most of it" further down cannot pass anything.
/// `None` when that line names no verdict; every caller reads that as not
/// achieved.
pub fn parse_verdict(output: &str) -> Option<GoalVerdict> {
    let lines: Vec<&str> = output.lines().collect();
    let idx = lines.iter().position(|line| !line.trim().is_empty())?;
    let stripped = lines[idx]
        .trim()
        .trim_start_matches(|c: char| !c.is_ascii_alphabetic());
    let upper = stripped.to_ascii_uppercase();
    for token in ["NOT ACHIEVED", "NOT_ACHIEVED", "NOT-ACHIEVED"] {
        if starts_with_token(&upper, token) {
            let rest = stripped[token.len()..]
                .trim_start_matches(|c: char| !c.is_alphanumeric() && c != '`' && c != '/')
                .trim();
            let mut feedback = rest.to_string();
            for line in &lines[idx + 1..] {
                if !feedback.is_empty() {
                    feedback.push('\n');
                }
                feedback.push_str(line.trim_end());
            }
            let feedback = feedback.trim();
            let feedback = if feedback.is_empty() {
                "the critic returned NOT ACHIEVED without saying why; re-check every requirement \
                 of the goal yourself, run the tests, and fix what fails"
            } else {
                feedback
            };
            return Some(GoalVerdict::NotAchieved(brief(
                feedback,
                MAX_FEEDBACK_CHARS,
            )));
        }
    }
    if starts_with_token(&upper, "ACHIEVED") {
        return Some(GoalVerdict::Achieved);
    }
    None
}

/// The text of the builder's last reply, if its last message has any.
pub fn last_reply(history: &[ChatMessage]) -> Option<String> {
    history
        .iter()
        .rev()
        .find(|message| message.role == Role::Assistant)
        .map(ChatMessage::text)
}

/// The gap text after a verdict token: whatever follows it on its own line,
/// then the following lines, joined and trimmed to one carryable line.
/// `token_len` is the length of the token just matched, which is ASCII.
fn gap_after(
    lines: &[&str],
    idx: usize,
    stripped_line: &str,
    token_len: usize,
    fallback: &str,
) -> String {
    let mut gap = String::new();
    // Remainder of the verdict line itself, after the token and any separator.
    let same_line = stripped_line[token_len..]
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
        gap.push_str(fallback);
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

/// How the builder is told the loop works. Shared by every goal prompt so the
/// instruction cannot drift between them.
const STOP_INSTRUCTION: &str = "Do not stop at a checkpoint to report progress; keep going. Stop \
     when you have verified the goal is fully achieved. Every time you stop, an independent \
     critic examines the project, and unless it signs off you get what it says needs to change \
     and keep working.";

/// The first turn of a freshly set goal.
pub fn goal_kickoff_prompt(goal: &str) -> String {
    format!(
        "A standing goal is set for this project:\n\n{goal}\n\nWork toward it now until it is \
         achieved. {STOP_INSTRUCTION}"
    )
}

/// The turn after one that ran out of steps.
pub fn goal_continue_prompt(goal: &str) -> String {
    format!(
        "You ran out of steps before you finished, so the goal loop is still running. The \
         goal:\n\n{goal}\n\nKeep working on what remains. {STOP_INSTRUCTION}"
    )
}

/// What the loop does after a goal turn ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalStep {
    /// The builder stopped: run the critic.
    Critique,
    /// It was cut off mid-work: send this prompt and keep going.
    Continue(String),
    /// End the loop and report why.
    Stop(String),
}

/// Decide the next move after a goal turn. `idle_turns` counts turns in a row,
/// this one included, that did no work.
///
/// A turn that ran out of steps did not stop, it was stopped, so there is
/// nothing finished to judge yet and a critic would be reading a half-made
/// change. Every turn that ended on its own gets the critic.
pub fn plan_after_goal_turn(goal: &str, reason: DoneReason, idle_turns: u32) -> GoalStep {
    match reason {
        DoneReason::Completed => GoalStep::Critique,
        DoneReason::MaxSteps if idle_turns >= GOAL_IDLE_LIMIT => {
            GoalStep::Stop(format!("{idle_turns} turns in a row did no work"))
        }
        DoneReason::MaxSteps => GoalStep::Continue(goal_continue_prompt(goal)),
        DoneReason::Stopped => GoalStep::Stop("the turn was stopped".to_string()),
        DoneReason::TimeLimit => GoalStep::Stop("the time limit was reached".to_string()),
        DoneReason::CircuitBreaker => {
            GoalStep::Stop("the circuit breaker tripped on repeated failures".to_string())
        }
    }
}

/// What the loop does with a critic's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalOutcome {
    /// The critic signed off.
    Achieved,
    /// Send the critic's feedback back to the builder.
    Rework(String),
    /// End the loop and report why.
    Stop(String),
}

/// Decide what a verdict means for the loop. `idle_turns` and `critic_failures`
/// are the current streaks, this verdict included.
///
/// The critic always gets to look first, so a turn that did nothing but finds
/// the goal already met still ends in `Achieved`.
pub fn plan_after_critique(
    goal: &str,
    verdict: &GoalVerdict,
    idle_turns: u32,
    critic_failures: u32,
) -> GoalOutcome {
    match plan_after_verdict(Claim::Finished(goal), verdict) {
        CriticAction::Accept => GoalOutcome::Achieved,
        CriticAction::Rework(_) if critic_failures >= GOAL_CRITIC_FAILURE_LIMIT => {
            GoalOutcome::Stop(format!(
                "the critic could not run {critic_failures} times in a row"
            ))
        }
        CriticAction::Rework(_) if idle_turns >= GOAL_IDLE_LIMIT => GoalOutcome::Stop(format!(
            "{idle_turns} turns in a row did no work and the critic still says it is not achieved"
        )),
        CriticAction::Rework(prompt) => GoalOutcome::Rework(prompt),
    }
}

/// What a verifier is asked to judge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim<'a> {
    /// The work was meant to finish. The text is the request or the goal, and
    /// the verifier judges whether it is satisfied.
    Finished(&'a str),
    /// One cycle of a continuous mission ended. The verifier judges whether
    /// `report`, the cycle's last reply, is true, using `mission` to know what
    /// the claims mean, not as a list the cycle had to finish.
    Cycle {
        /// The standing mission, verbatim.
        mission: &'a str,
        /// What the cycle said it did.
        report: &'a str,
    },
}

impl<'a> Claim<'a> {
    /// The claim a headless run makes when a turn ends: the whole `goal` for a
    /// run meant to finish, the turn's `report` for a continuous one.
    pub fn for_run(continuous: bool, goal: &'a str, report: &'a str) -> Self {
        if continuous {
            Self::Cycle {
                mission: goal,
                report,
            }
        } else {
            Self::Finished(goal)
        }
    }
}

/// What both verifiers check on a continuous cycle. One text, so the review
/// and the critic cannot drift apart on what an honest partial report is.
const CYCLE_CHECK: &str = "\
The agent is working through a long standing mission in cycles. The end of a \
cycle is not a claim that the mission is finished. Most cycles finish one \
piece of it and say what remains, and that is what an honest report looks \
like. Do not judge whether the mission is done. Judge whether the report is \
true.\n\
\n\
1. List every concrete claim the report makes: work committed or merged, files \
and directories that exist, tests or commands that pass, numbers it quotes, \
results it says it produced, and any claim that the mission or a named part of \
it is complete.\n\
2. Check each one on the machine. Run the tests and commands it says pass. Read \
what it says it wrote. A false claim is a failure, and so is a stub or \
placeholder presented as working, a hardcoded or faked result, or a test or \
check weakened so that it passes.\n\
3. Work the report says is unfinished is not a failure. Unfinished work fails \
only when the report says it is done.\n\
\n\
Use the mission to understand what the claims mean and what a real result \
looks like, not as a checklist this cycle had to complete.";

/// The critic's system prompt for a continuous cycle.
fn cycle_critic_system_prompt() -> String {
    format!(
        "You are a harsh, independent critic. An agent on a long mission just ended a cycle \
         and reported what it did. You never saw it work and you get none of its reasoning: \
         only the mission, its report, and the project on disk. Your default position is that \
         something in the report is wrong, and your job is to find out what.\n\n{CYCLE_CHECK}\n\
         4. For at least one thing the report says works, try an input or case the agent \
         probably did not try.\n\n\
         You judge, you do not build. Do not write, edit or delete anything.\n\n\
         The FIRST line of your reply is your verdict, exactly one of:\n  \
         ACHIEVED       every claim in the report holds and you ran something that shows it\n  \
         NOT ACHIEVED   anything less\n\n\
         After NOT ACHIEVED, say which claims are false: each one, the evidence (a path, a \
         command and its output), and what has to change. Most important first. No praise, no \
         summary of what is fine, and nothing about work the report already says is not done."
    )
}

/// The reviewer's system prompt for a continuous cycle.
fn cycle_review_system_prompt() -> String {
    format!(
        "You are reviewing one cycle of a continuous autonomous run. You never saw the agent \
         work and you get none of its reasoning: only the mission, the report it ended the \
         cycle with, and the machine it left behind.\n\n{CYCLE_CHECK}\n\n\
         Judge, do not build. Do not write, edit, or delete anything, and do not finish the \
         work yourself.\n\n\
         The FIRST line of your reply is your verdict, exactly one of:\n  \
         PASS   what the report claims is true, and you ran something that shows it\n  \
         FAIL   it is not; on the next line, say which claim is false and quote the command \
         output that shows it, in one or two sentences\n\n\
         No praise, no summary of the agent's work. If you could not check a claim at all, \
         that is FAIL, and say what stopped you."
    )
}

/// The task handed to either verifier on a continuous cycle.
fn cycle_task(mission: &str, report: &str, project_root: &Path) -> String {
    let report = if report.trim().is_empty() {
        "(no report: the cycle ended without saying what it did. Check what it changed and \
         that the project still builds and its tests still pass.)"
    } else {
        report
    };
    format!(
        "The standing mission, verbatim between the markers:\n\n<mission>\n{mission}\n</mission>\
         \n\nThe report the agent ended this cycle with, verbatim between the markers:\n\n\
         <report>\n{report}\n</report>\n\nIt ran in {root}. Check whether the report is true \
         and return your verdict now.",
        root = project_root.display()
    )
}

/// The critic's definition and task for `claim`.
pub fn critic_run(claim: Claim<'_>, project_root: &Path) -> (SubagentConfig, String) {
    match claim {
        Claim::Finished(goal) => (critic_config(), critic_task(goal, project_root)),
        Claim::Cycle { mission, report } => (
            SubagentConfig {
                system_prompt: cycle_critic_system_prompt(),
                ..critic_config()
            },
            cycle_task(mission, report, project_root),
        ),
    }
}

/// The reviewer's definition and task for `claim`.
pub fn review_run(claim: Claim<'_>, project_root: &Path) -> (SubagentConfig, String) {
    match claim {
        Claim::Finished(request) => (review_config(), review_task(request, project_root)),
        Claim::Cycle { mission, report } => (
            SubagentConfig {
                system_prompt: cycle_review_system_prompt(),
                ..review_config()
            },
            cycle_task(mission, report, project_root),
        ),
    }
}

/// The turn after a verifier found a cycle's report untrue. It names the
/// false claims and says plainly that partial progress is not the problem,
/// because "you said it was done and it is not" is what taught a builder to
/// fake a finished mission.
fn cycle_rework(found: &str) -> String {
    format!(
        "An independent check of this cycle's report found claims in it that are not true:\n\n\
         {found}\n\n\
         Fix the work so those claims hold, or correct the report to say what is actually \
         done. You are not expected to finish the mission in one cycle, and a report of honest \
         partial progress passes. Do not weaken a test or a check, or hardcode a result, to \
         make a claim pass."
    )
}

/// What the loop should do with a verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CriticAction {
    /// The goal is achieved: let the cycle land.
    Accept,
    /// Not yet: send this prompt back to the builder.
    Rework(String),
}

/// Decide the loop's next move from a verdict. Pure so the loop's control flow
/// can be tested without a model.
pub fn plan_after_verdict(claim: Claim<'_>, verdict: &GoalVerdict) -> CriticAction {
    match (verdict, claim) {
        (GoalVerdict::Achieved, _) => CriticAction::Accept,
        (GoalVerdict::NotAchieved(feedback), Claim::Cycle { .. }) => {
            CriticAction::Rework(cycle_rework(feedback))
        }
        (GoalVerdict::NotAchieved(feedback), Claim::Finished(goal)) => {
            CriticAction::Rework(format!(
                "You stopped, and an independent critic checked the project. It says the goal is \
             NOT achieved. What it says needs to change:\n\n{feedback}\n\nThe goal:\n\n\
             {goal}\n\nAddress every point, not just the first. Run the build and tests \
             yourself. The critic decides whether the goal is met, not you. {STOP_INSTRUCTION}"
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// The completion review
// ---------------------------------------------------------------------------

/// Steps the review may take: enough to list a directory, read the deliverables
/// and run the acceptance command a couple of times, not enough to start
/// building. A review that needs forty steps is doing the work again.
const REVIEW_MAX_STEPS: u32 = 24;

/// Rounds of review feedback one claim of done may cost.
///
/// One. The review catches the run that wrote nothing and said it was finished,
/// and one round of feedback is what fixes that. Past one it stops being a
/// check and becomes an argument: the same two models trading "it is done" and
/// "no it is not" at a model call each, on a run with nobody watching. Whatever
/// is still wrong after the rework is wrong in a way another round of the same
/// prompt was not going to find.
pub const REVIEW_MAX_ROUNDS: u32 = 1;

/// Seconds of the run's own deadline that must be left for a review to start.
///
/// The review runs a model call and then some commands, and a review that is
/// killed halfway has cost the run its last minute and told it nothing. Under
/// this, the claim is accepted as it stands.
pub const REVIEW_MIN_SECS: u64 = 60;

/// A turn shorter than this is not sent back for a self-check.
///
/// The check costs a model turn. Measured against a suite of quick fixes it
/// found nothing and cost a third of the turn again; on turns past this it
/// found a missed requirement about one time in twelve.
pub const SELF_CHECK_MIN_SECS: u64 = 25;

/// What the agent is asked when its own turn is sent back for a second look.
///
/// Same conversation, so the files it read are still in context and the check
/// is a fraction of an independent review's cost. It is the builder grading
/// itself, which is why it is told what counts as evidence.
pub const SELF_CHECK_PROMPT: &str = "\
Before this is final, verify it once. For each behaviour the request, the spec \
or the README states, compare it with the code that implements it, including \
code you did not change: passing visible tests is not evidence. Run one or two \
quick cases the tests do not cover, in a single command. Fix whatever disagrees \
and say in a line what you fixed. If nothing needed fixing, reply with only: \
Verified, no changes.";

/// Whether to send a finished turn back for a self-check.
///
/// `enabled` is the `self_check` config key. One check per request, only for a
/// turn that changed something, and only for one long enough to be worth it.
pub fn plan_self_check(enabled: bool, effects: u64, rounds_used: u32, turn_secs: u64) -> bool {
    enabled && effects > 0 && rounds_used == 0 && turn_secs >= SELF_CHECK_MIN_SECS
}

/// The reviewer's system prompt.
///
/// The four numbered steps are the four ways a confident "done" has been wrong
/// in practice: the deliverable was never written, the check was a variation of
/// the one the user asked for, the check ran against the agent's own example,
/// and the thing that made the check pass was torn down afterwards.
const REVIEW_SYSTEM_PROMPT: &str = "\
You are reviewing a finished autonomous run. You never saw the agent work and \
you get none of its reasoning, its summary, or its claims: only the request it \
was given and the machine it left behind. Answer one question: is the request \
satisfied right now, on this machine?\n\
\n\
Work through these in order.\n\
1. Restate the request in your own words, and list every deliverable it names.\n\
2. Check every one of them on the machine: a file, a directory, a path, an \
endpoint, a service. Missing, zero bytes, or still a placeholder is a failure \
on its own, and say which one it was.\n\
3. If the request states how the work will be checked (a command to run, a \
scenario to reproduce, an input with an expected output), run it literally, as \
written, from where the request says to run it. Do not substitute a variation \
you believe is equivalent.\n\
4. Ask whether the result still holds when the inputs change. If the work could \
have been fitted to one example, construct a second case the request also \
covers and run that too. If anything was reset, restarted, moved, or cleaned up \
after the agent's own check, redo the check now.\n\
\n\
Judge, do not build. Do not write, edit, or delete anything, and do not finish \
the work yourself. Your job is to report what is there, not to repair it.\n\
\n\
The FIRST line of your reply is your verdict, exactly one of:\n\
  PASS   the request is satisfied, and you ran something that shows it\n\
  FAIL   it is not; on the next line, say what is wrong and quote the command \
output that shows it, in one or two sentences\n\
\n\
No praise, no summary of the agent's work, no advice beyond what is wrong. \
Absence of evidence is FAIL: if you could not check a deliverable at all, that \
is FAIL, and say what stopped you.";

/// The review subagent's definition.
///
/// `tool_scope` is left open and the narrowing happens at spawn time
/// ([`crate::agent::subagent::RunScope::Inspect`]), so the reviewer keeps every
/// inspection tool this install has while having none that writes a file.
pub fn review_config() -> SubagentConfig {
    SubagentConfig {
        name: "completion-review".to_string(),
        description: "Independent review of a finished autonomous run: checks the request was \
                      actually satisfied and returns PASS or FAIL."
            .to_string(),
        tool_scope: None,
        system_prompt: REVIEW_SYSTEM_PROMPT.to_string(),
        max_steps: StepBudget::new(REVIEW_MAX_STEPS),
    }
}

/// The task handed to the reviewer: the request, verbatim, and where it ran.
///
/// Verbatim matters. A summary of the request is the agent's own reading of it,
/// and the agent's reading is the thing under review.
pub fn review_task(request: &str, project_root: &Path) -> String {
    format!(
        "This is the request the agent was given, verbatim between the markers. The agent has \
         reported it complete.\n\n<request>\n{request}\n</request>\n\nIt ran in {root}. Check \
         whether the request is satisfied there and return your verdict now.",
        root = project_root.display()
    )
}

/// A reviewer's verdict on one claim of done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewVerdict {
    /// The request is satisfied and the reviewer saw evidence of it.
    Pass,
    /// It is not, and this is what is wrong.
    Fail(String),
    /// The reviewer named no verdict, so the claim stands unreviewed.
    Unclear,
}

impl ReviewVerdict {
    /// A one-line description for a notice or a mission note.
    pub fn summary(&self) -> String {
        match self {
            Self::Pass => "PASS — the review found the request satisfied".to_string(),
            Self::Fail(why) => format!("FAIL — {why}"),
            Self::Unclear => "no verdict, so the claim of done was not checked".to_string(),
        }
    }
}

/// Parse a reviewer's reply. `None` when it named no verdict. The caller
/// decides what an unclear reviewer means, and no caller reads it as a pass.
pub fn parse_review(output: &str) -> Option<ReviewVerdict> {
    let lines: Vec<&str> = output.lines().collect();
    for (idx, line) in lines.iter().enumerate() {
        let stripped = line
            .trim()
            .trim_start_matches(|c: char| !c.is_ascii_alphabetic());
        let upper = stripped.to_ascii_uppercase();
        let (upper, stripped) = match upper.strip_prefix("VERDICT") {
            Some(rest) => {
                let rest = rest.trim_start_matches(|c: char| !c.is_ascii_alphabetic());
                (rest.to_string(), &stripped[stripped.len() - rest.len()..])
            }
            None => (upper, stripped),
        };
        if starts_with_token(&upper, "PASS") {
            return Some(ReviewVerdict::Pass);
        }
        if starts_with_token(&upper, "FAIL") {
            return Some(ReviewVerdict::Fail(gap_after(
                &lines,
                idx,
                stripped,
                "FAIL".len(),
                "the review returned FAIL without saying what is wrong; check the request's \
                 deliverables yourself before reporting done again",
            )));
        }
    }
    None
}

/// What the run loop should do after a review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewAction {
    /// Let the run finish.
    Accept,
    /// Hand this back to the agent for one more turn.
    Rework(String),
}

/// Decide the loop's next move from a review verdict. `rounds_used` counts the
/// reviews already spent on this claim, including this one.
pub fn plan_after_review(
    claim: Claim<'_>,
    verdict: &ReviewVerdict,
    rounds_used: u32,
) -> ReviewAction {
    match verdict {
        // A reviewer with nothing to say has given the agent nothing to fix.
        ReviewVerdict::Pass | ReviewVerdict::Unclear => ReviewAction::Accept,
        // Out of rounds: the run ends with the failure reported rather than
        // argued. The caller is expected to say so on its way out.
        ReviewVerdict::Fail(_) if rounds_used > REVIEW_MAX_ROUNDS => ReviewAction::Accept,
        ReviewVerdict::Fail(why) if matches!(claim, Claim::Cycle { .. }) => {
            ReviewAction::Rework(cycle_rework(why))
        }
        ReviewVerdict::Fail(why) => ReviewAction::Rework(format!(
            "You reported this done. An independent review checked the machine and it is not:\n\n\
             {why}\n\n\
             Fix exactly that, then report done again. Run the request's own check yourself \
             before you do, and leave whatever made it pass in place. The next thing to \
             look at it will not be you."
        )),
    }
}

/// Why a completion review did not run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewSkip {
    /// Turned off for this surface (see [`review_enabled`]).
    Disabled,
    /// The turn wrote no file and ran no command, so there is no work to check
    /// and a review would be a model call spent reading an unchanged tree.
    NothingHappened,
    /// This claim has already had its review and its round of feedback.
    RoundsSpent,
    /// Less than [`REVIEW_MIN_SECS`] of the run's deadline is left.
    NoTimeLeft,
}

impl ReviewSkip {
    /// A one-line reason, for a log or a `--output-format text` notice.
    pub fn summary(self) -> &'static str {
        match self {
            Self::Disabled => "completion review is off",
            Self::NothingHappened => "nothing was written or run this turn",
            Self::RoundsSpent => "the completion review has already had its round",
            Self::NoTimeLeft => "the run's time limit is too close",
        }
    }
}

/// Whether to review this claim of done, or why not.
///
/// Pure, and the whole decision: every caller asks this one function so the
/// skip rules cannot drift between surfaces.
pub fn plan_review(
    enabled: bool,
    effects: u64,
    rounds_used: u32,
    secs_left: Option<u64>,
) -> Result<(), ReviewSkip> {
    if !enabled {
        return Err(ReviewSkip::Disabled);
    }
    if rounds_used >= REVIEW_MAX_ROUNDS {
        return Err(ReviewSkip::RoundsSpent);
    }
    if effects == 0 {
        return Err(ReviewSkip::NothingHappened);
    }
    if secs_left.is_some_and(|left| left < REVIEW_MIN_SECS) {
        return Err(ReviewSkip::NoTimeLeft);
    }
    Ok(())
}

/// Is the completion review on for this surface?
///
/// `configured` is the `completion_review` config key, unset by default so the
/// answer can depend on who is watching. Nobody is watching an autonomous run,
/// so it reviews; in the TUI the user is reading every line as it arrives and
/// an extra model call per turn is their time and their money, so it does not.
/// Setting the key either way wins on both.
pub fn review_enabled(configured: Option<bool>, interactive: bool) -> bool {
    configured.unwrap_or(!interactive)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_bare_verdict() {
        assert_eq!(parse_verdict("ACHIEVED"), Some(GoalVerdict::Achieved));
        assert_eq!(parse_verdict("**ACHIEVED**"), Some(GoalVerdict::Achieved));
        assert!(matches!(
            parse_verdict("NOT ACHIEVED"),
            Some(GoalVerdict::NotAchieved(_))
        ));
    }

    #[test]
    fn not_achieved_is_never_read_as_achieved() {
        assert!(matches!(
            parse_verdict("**NOT ACHIEVED**\n- tests fail"),
            Some(GoalVerdict::NotAchieved(_))
        ));
    }

    #[test]
    fn not_achieved_carries_all_of_the_feedback() {
        let reply = "NOT ACHIEVED\n1. `cargo test` fails: 3 failed in parser.rs\n\n\
                     2. src/retry.rs has no backoff, so a flapping host is hammered.";
        let Some(GoalVerdict::NotAchieved(feedback)) = parse_verdict(reply) else {
            panic!("expected NOT ACHIEVED");
        };
        assert!(feedback.contains("3 failed in parser.rs"), "{feedback}");
        assert!(
            feedback.contains("no backoff"),
            "past a blank line: {feedback}"
        );
    }

    #[test]
    fn feedback_on_the_verdict_line_is_kept() {
        assert_eq!(
            parse_verdict("NOT ACHIEVED: missing the error path for a closed socket"),
            Some(GoalVerdict::NotAchieved(
                "missing the error path for a closed socket".to_string()
            ))
        );
    }

    #[test]
    fn only_the_first_line_is_the_verdict() {
        // Prose that happens to start a later line with the word cannot pass.
        assert_eq!(parse_verdict("Looking at it.\nACHIEVED"), None);
        assert_eq!(
            parse_verdict("\n\nACHIEVED\nall tests pass"),
            Some(GoalVerdict::Achieved)
        );
        assert_eq!(parse_verdict("Achievedness is unclear"), None);
    }

    #[test]
    fn not_achieved_without_feedback_still_says_something() {
        let Some(GoalVerdict::NotAchieved(feedback)) = parse_verdict("NOT ACHIEVED") else {
            panic!("expected NOT ACHIEVED");
        };
        assert!(!feedback.is_empty());
    }

    #[test]
    fn long_feedback_is_trimmed() {
        let huge = format!("NOT ACHIEVED\n{}", "x".repeat(MAX_FEEDBACK_CHARS + 200));
        let Some(GoalVerdict::NotAchieved(feedback)) = parse_verdict(&huge) else {
            panic!("expected NOT ACHIEVED");
        };
        assert!(feedback.chars().count() <= MAX_FEEDBACK_CHARS + 1);
    }

    #[test]
    fn every_turn_that_stops_goes_to_the_critic() {
        // Idle or not: the critic looks first, so a turn that did nothing on a
        // goal already met still ends the loop as achieved.
        for idle in [0, 1, GOAL_IDLE_LIMIT, GOAL_IDLE_LIMIT + 5] {
            assert_eq!(
                plan_after_goal_turn("g", DoneReason::Completed, idle),
                GoalStep::Critique
            );
        }
    }

    #[test]
    fn a_turn_cut_off_by_its_step_budget_keeps_working() {
        let GoalStep::Continue(prompt) =
            plan_after_goal_turn("ship the parser", DoneReason::MaxSteps, 0)
        else {
            panic!("expected continue");
        };
        assert!(prompt.contains("ship the parser"));
        assert!(prompt.contains("independent critic"));
        assert!(matches!(
            plan_after_goal_turn("g", DoneReason::MaxSteps, GOAL_IDLE_LIMIT),
            GoalStep::Stop(_)
        ));
    }

    #[test]
    fn an_interrupted_or_failed_turn_ends_the_loop_without_a_critic() {
        for reason in [
            DoneReason::Stopped,
            DoneReason::TimeLimit,
            DoneReason::CircuitBreaker,
        ] {
            assert!(matches!(
                plan_after_goal_turn("g", reason, 0),
                GoalStep::Stop(_)
            ));
        }
    }

    #[test]
    fn the_critics_sign_off_ends_the_loop_whatever_the_streaks() {
        assert_eq!(
            plan_after_critique("g", &GoalVerdict::Achieved, GOAL_IDLE_LIMIT, 0),
            GoalOutcome::Achieved
        );
    }

    #[test]
    fn a_rejection_sends_the_feedback_back_while_the_loop_is_moving() {
        let verdict = GoalVerdict::NotAchieved("parser.rs panics on empty input".into());
        let GoalOutcome::Rework(prompt) =
            plan_after_critique("ship the parser", &verdict, GOAL_IDLE_LIMIT - 1, 0)
        else {
            panic!("expected rework");
        };
        assert!(prompt.contains("parser.rs panics on empty input"));
    }

    #[test]
    fn a_builder_that_stops_doing_anything_cannot_loop_forever() {
        let verdict = GoalVerdict::NotAchieved("nothing changed".into());
        assert!(matches!(
            plan_after_critique("g", &verdict, GOAL_IDLE_LIMIT, 0),
            GoalOutcome::Stop(_)
        ));
    }

    #[test]
    fn a_critic_that_keeps_failing_stops_the_loop() {
        let verdict = GoalVerdict::NotAchieved("could not run".into());
        assert!(matches!(
            plan_after_critique("g", &verdict, 0, GOAL_CRITIC_FAILURE_LIMIT - 1),
            GoalOutcome::Rework(_)
        ));
        let GoalOutcome::Stop(why) =
            plan_after_critique("g", &verdict, 0, GOAL_CRITIC_FAILURE_LIMIT)
        else {
            panic!("expected stop");
        };
        assert!(why.contains("could not run"), "{why}");
    }

    #[test]
    fn achieved_accepts() {
        assert_eq!(
            plan_after_verdict(Claim::Finished("g"), &GoalVerdict::Achieved),
            CriticAction::Accept
        );
    }

    #[test]
    fn not_achieved_sends_the_critics_feedback_back() {
        let CriticAction::Rework(prompt) = plan_after_verdict(
            Claim::Finished("ship the parser"),
            &GoalVerdict::NotAchieved("parser.rs panics on empty input".into()),
        ) else {
            panic!("expected rework");
        };
        assert!(prompt.contains("parser.rs panics on empty input"));
        assert!(prompt.contains("ship the parser"));
        assert!(prompt.contains("independent critic"));
        assert!(!prompt.contains("You claimed"), "{prompt}");
    }

    // --- continuous cycles --------------------------------------------------

    const MISSION: &str = "write all S1/S2 code and pass V0 to V10 by 2026-12-31";
    const REPORT: &str = "Merged A1, B1, B5 and F3 at 085c625. pytest: 283 passed. \
                          V stages not started.";

    #[test]
    fn a_continuous_turn_claims_its_report_and_a_finite_run_claims_the_goal() {
        assert_eq!(
            Claim::for_run(true, MISSION, REPORT),
            Claim::Cycle {
                mission: MISSION,
                report: REPORT
            }
        );
        assert_eq!(
            Claim::for_run(false, MISSION, REPORT),
            Claim::Finished(MISSION)
        );
    }

    #[test]
    fn a_finished_claim_gets_exactly_the_verifiers_it_always_had() {
        let root = Path::new("/work");
        let (config, task) = review_run(Claim::Finished("serve it on :8080"), root);
        assert_eq!(config.system_prompt, review_config().system_prompt);
        assert_eq!(task, review_task("serve it on :8080", root));
        let (config, task) = critic_run(Claim::Finished("ship the parser"), root);
        assert_eq!(config.system_prompt, critic_config().system_prompt);
        assert_eq!(task, critic_task("ship the parser", root));
    }

    #[test]
    fn a_cycle_is_judged_on_its_report_not_on_the_whole_mission() {
        let root = Path::new("/work");
        let claim = Claim::Cycle {
            mission: MISSION,
            report: REPORT,
        };
        for (config, task, finished_prompt) in [
            {
                let (c, t) = review_run(claim, root);
                (c, t, review_config().system_prompt)
            },
            {
                let (c, t) = critic_run(claim, root);
                (c, t, critic_config().system_prompt)
            },
        ] {
            // The report is what is under review, so it has to reach the
            // verifier whole; the mission rides along as context.
            assert!(
                task.contains(&format!("<report>\n{REPORT}\n</report>")),
                "{task}"
            );
            assert!(task.contains(MISSION), "{task}");
            assert!(!task.contains("claims it is achieved"), "{task}");
            assert!(!task.contains("reported it complete"), "{task}");
            assert_ne!(config.system_prompt, finished_prompt);
            assert!(config.system_prompt.contains(CYCLE_CHECK));
        }
        // Same budgets and names as the finished-run verifiers: only the
        // question changes.
        assert_eq!(
            review_run(claim, root).0.max_steps,
            review_config().max_steps
        );
        assert_eq!(critic_run(claim, root).0.name, critic_config().name);
    }

    #[test]
    fn a_cycle_that_ends_without_a_report_still_gets_something_to_check() {
        let (_, task) = review_run(
            Claim::Cycle {
                mission: MISSION,
                report: "  \n",
            },
            Path::new("/work"),
        );
        assert!(task.contains("no report"), "{task}");
    }

    #[test]
    fn a_false_cycle_report_is_sent_back_without_calling_it_a_finished_mission() {
        let claim = Claim::Cycle {
            mission: MISSION,
            report: REPORT,
        };
        let why = "`pytest` shows 3 failed, not 283 passed";
        let ReviewAction::Rework(review) =
            plan_after_review(claim, &ReviewVerdict::Fail(why.into()), REVIEW_MAX_ROUNDS)
        else {
            panic!("a failed cycle review sends it back");
        };
        let CriticAction::Rework(critic) =
            plan_after_verdict(claim, &GoalVerdict::NotAchieved(why.into()))
        else {
            panic!("a failed cycle critique sends it back");
        };
        for prompt in [review, critic] {
            assert!(prompt.contains(why), "{prompt}");
            assert!(prompt.contains("partial progress passes"), "{prompt}");
            // The phrasing that sent the builder hunting for a finish line.
            assert!(!prompt.contains("report done again"), "{prompt}");
            assert!(!prompt.contains("request's own check"), "{prompt}");
            assert!(!prompt.contains("claimed the goal is achieved"), "{prompt}");
        }
        // A true report lands, and a cycle review still gets only its one round.
        assert_eq!(
            plan_after_review(claim, &ReviewVerdict::Pass, 1),
            ReviewAction::Accept
        );
        assert_eq!(
            plan_after_verdict(claim, &GoalVerdict::Achieved),
            CriticAction::Accept
        );
        assert_eq!(
            plan_after_review(
                claim,
                &ReviewVerdict::Fail(why.into()),
                REVIEW_MAX_ROUNDS + 1
            ),
            ReviewAction::Accept
        );
    }

    #[test]
    fn kickoff_tells_the_builder_a_critic_examines_every_stop() {
        let prompt = goal_kickoff_prompt("rewrite spore in assembly");
        assert!(prompt.contains("rewrite spore in assembly"));
        assert!(prompt.contains("Every time you stop"));
    }

    // --- the completion review ---------------------------------------------

    #[test]
    fn parses_a_review_verdict() {
        assert_eq!(parse_review("PASS"), Some(ReviewVerdict::Pass));
        assert_eq!(
            parse_review("**PASS** — server.py answers on :8080"),
            Some(ReviewVerdict::Pass)
        );
        assert_eq!(
            parse_review("FAIL\n/app/out.txt does not exist."),
            Some(ReviewVerdict::Fail(
                "/app/out.txt does not exist.".to_string()
            ))
        );
        assert_eq!(
            parse_review("FAIL: `curl -sf localhost:8080` returned 404"),
            Some(ReviewVerdict::Fail(
                "`curl -sf localhost:8080` returned 404".to_string()
            ))
        );
        assert_eq!(parse_review("**Verdict: PASS**"), Some(ReviewVerdict::Pass));
        assert_eq!(
            parse_review("Verdict: FAIL: out.txt is empty"),
            Some(ReviewVerdict::Fail("out.txt is empty".to_string()))
        );
    }

    #[test]
    fn a_review_that_names_no_verdict_is_unclear() {
        assert_eq!(parse_review("Looks good to me, mostly."), None);
        assert_eq!(parse_review("FAILURE-mode analysis follows"), None);
    }

    #[test]
    fn an_unclear_review_is_not_a_pass_and_sends_nothing_back() {
        assert_eq!(
            plan_after_review(Claim::Finished("r"), &ReviewVerdict::Unclear, 1),
            ReviewAction::Accept
        );
        assert!(!ReviewVerdict::Unclear.summary().contains("PASS"));
    }

    #[test]
    fn a_review_that_fails_without_saying_why_still_says_something() {
        let Some(ReviewVerdict::Fail(why)) = parse_review("FAIL") else {
            panic!("expected FAIL");
        };
        assert!(!why.is_empty());
    }

    #[test]
    fn a_passing_review_lets_the_run_finish() {
        assert_eq!(
            plan_after_review(Claim::Finished("r"), &ReviewVerdict::Pass, 1),
            ReviewAction::Accept
        );
    }

    #[test]
    fn a_failing_review_buys_exactly_one_rework() {
        let verdict = ReviewVerdict::Fail("/app/out.txt is missing".to_string());
        let ReviewAction::Rework(prompt) =
            plan_after_review(Claim::Finished("r"), &verdict, REVIEW_MAX_ROUNDS)
        else {
            panic!("the first failed review sends it back");
        };
        assert!(prompt.contains("/app/out.txt is missing"));
        assert!(prompt.contains("report done again"));
        // The round after that is an argument, not a check.
        assert_eq!(
            plan_after_review(Claim::Finished("r"), &verdict, REVIEW_MAX_ROUNDS + 1),
            ReviewAction::Accept
        );
    }

    #[test]
    fn a_turn_that_wrote_nothing_and_ran_nothing_is_not_reviewed() {
        assert_eq!(
            plan_review(true, 0, 0, None),
            Err(ReviewSkip::NothingHappened)
        );
        assert_eq!(plan_review(true, 1, 0, None), Ok(()));
    }

    #[test]
    fn a_claim_is_reviewed_once() {
        assert_eq!(plan_review(true, 5, 0, None), Ok(()));
        assert_eq!(
            plan_review(true, 5, REVIEW_MAX_ROUNDS, None),
            Err(ReviewSkip::RoundsSpent)
        );
    }

    #[test]
    fn the_review_does_not_start_on_the_last_seconds_of_a_run() {
        assert_eq!(
            plan_review(true, 5, 0, Some(REVIEW_MIN_SECS - 1)),
            Err(ReviewSkip::NoTimeLeft)
        );
        assert_eq!(plan_review(true, 5, 0, Some(REVIEW_MIN_SECS)), Ok(()));
        // A run with no `--max-hours` has no last seconds.
        assert_eq!(plan_review(true, 5, 0, None), Ok(()));
    }

    #[test]
    fn a_self_check_is_for_one_long_turn_that_changed_something() {
        assert!(plan_self_check(true, 3, 0, SELF_CHECK_MIN_SECS));
        assert!(!plan_self_check(false, 3, 0, 600));
        assert!(!plan_self_check(true, 0, 0, 600));
        assert!(!plan_self_check(true, 3, 1, 600));
        assert!(!plan_self_check(true, 3, 0, SELF_CHECK_MIN_SECS - 1));
    }

    #[test]
    fn off_is_off_whatever_else_is_true() {
        assert_eq!(plan_review(false, 99, 0, None), Err(ReviewSkip::Disabled));
    }

    #[test]
    fn the_review_is_on_where_nobody_is_watching_and_off_where_someone_is() {
        assert!(review_enabled(None, false), "headless reviews by default");
        assert!(
            !review_enabled(None, true),
            "the TUI does not: the user is reading the claim themselves"
        );
        // An explicit key wins on both surfaces.
        assert!(review_enabled(Some(true), true));
        assert!(!review_enabled(Some(false), false));
    }
}
