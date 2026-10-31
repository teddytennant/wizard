//! Plan mode's exit hatch: the `exit_plan` tool.
//!
//! While plan mode is active the dispatcher blocks every non-read-only tool
//! (see `crate::dispatch`); `exit_plan` is the one sanctioned way out. The
//! model calls it with the finished plan, the plan is persisted to
//! `<project>/.wizard/plan.md`, and an [`AgentEvent::PlanReady`] asks the
//! surface for a verdict: the TUI renders a review, headless runners and the
//! gateway auto-approve. Approval flips plan mode off and tells the model to
//! execute; rejection feeds the reviewer's feedback back as an ordinary tool
//! error and plan mode stays on.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agent::{AgentEvent, PlanGate, PlanVerdict};

use super::{Tool, ToolContext, ToolError, ToolOutput, parse_args};

/// Advertised name of the tool; the dispatcher's plan gate exempts it.
pub const EXIT_PLAN_TOOL_NAME: &str = "exit_plan";

/// Where the plan markdown is persisted, relative to the project root.
pub const PLAN_FILE: &str = ".wizard/plan.md";

/// `exit_plan` — present the finished plan for review and leave plan mode on
/// approval. Registered always (so the model can see it documented), but it
/// only does anything while plan mode is active.
pub struct ExitPlanTool {
    /// Plan-mode flag shared with the agent and its dispatcher; approval
    /// clears it.
    plan_mode: Arc<AtomicBool>,
    /// Omakase (chef's-choice) flag shared with the agent. When set, the plan
    /// is auto-approved with no review round-trip: the chef decides and
    /// proceeds. Cleared alongside plan mode on approval.
    omakase: Arc<AtomicBool>,
}

impl ExitPlanTool {
    pub fn new(plan_mode: Arc<AtomicBool>, omakase: Arc<AtomicBool>) -> Self {
        Self { plan_mode, omakase }
    }

    /// Persist the plan markdown to `<project>/.wizard/plan.md`.
    fn persist(&self, ctx: &ToolContext, plan: &str) -> Result<(), ToolError> {
        let path = ctx.cwd.join(PLAN_FILE);
        path.parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&path, plan))
            .map_err(|err| ToolError::Execution {
                tool: EXIT_PLAN_TOOL_NAME.to_string(),
                source: anyhow::anyhow!("writing {}: {err}", path.display()),
            })
    }
}

#[async_trait]
impl Tool for ExitPlanTool {
    fn name(&self) -> &str {
        EXIT_PLAN_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Finish plan mode by presenting your implementation plan for review. \
         Call this only while in plan mode, after investigating with read-only \
         tools. The plan is saved to .wizard/plan.md and reviewed; if approved \
         you may execute it, if rejected you receive feedback and stay in plan \
         mode."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "plan": {
                    "type": "string",
                    "description": "The full implementation plan, as markdown"
                }
            },
            "required": ["plan"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        #[derive(Deserialize)]
        struct Args {
            plan: String,
        }
        let Args { plan } = parse_args(EXIT_PLAN_TOOL_NAME, args)?;

        if !self.plan_mode.load(Ordering::SeqCst) {
            return Ok(ToolOutput::error(
                "not in plan mode — exit_plan only applies while plan mode is active",
            ));
        }

        // Omakase: chef's choice. There is no review gate — persist the plan,
        // clear the flags, tell the surface the chef is proceeding (best
        // effort; this is informational), and let the agent execute. This
        // path also covers contexts with no surface (subagents), where a
        // review round-trip would otherwise be impossible.
        if self.omakase.load(Ordering::SeqCst) {
            self.persist(ctx, &plan)?;
            self.plan_mode.store(false, Ordering::SeqCst);
            self.omakase.store(false, Ordering::SeqCst);
            if let Some(events) = ctx.events.clone() {
                let _ = events.send(AgentEvent::OmakaseProceeding { plan }).await;
            }
            return Ok(ToolOutput::ok(
                "Omakase — plan auto-approved (chef's choice). Plan mode is off; \
                 execute the plan end to end now.",
            ));
        }

        let Some(events) = ctx.events.clone() else {
            // Outside the dispatch pipeline there is no surface to review the
            // plan (e.g. a subagent context); refuse rather than silently
            // approve.
            return Ok(ToolOutput::error(
                "plan review is unavailable in this context; continue in plan mode",
            ));
        };

        // Persist the plan before asking for a verdict, so it survives a
        // rejected or interrupted review.
        self.persist(ctx, &plan)?;

        let (gate, verdict) = PlanGate::open();
        if events
            .send(AgentEvent::PlanReady {
                plan: plan.clone(),
                gate,
            })
            .await
            .is_err()
        {
            // The surface is gone; the turn is ending anyway. Close the gate
            // so the desk is not left holding a verdict channel nobody can
            // reach.
            gate.cancel();
            return Ok(ToolOutput::error(
                "plan review was cancelled (no surface to approve it); still in plan mode",
            ));
        }

        // Park until the reviewer answers, or until the surface goes away.
        // The verdict channel now waits at the gate desk rather than inside the
        // event, so a consumer that drops the event mid-review no longer
        // releases this call by dropping the sender with it. A closed event
        // channel is the same news arriving one step earlier: there is nobody
        // left to review anything.
        // A cancelled turn ends the review too: a surface that never answers
        // must not hold the turn open past Esc or `session/cancel`.
        let verdict = tokio::select! {
            answer = verdict => answer.ok(),
            () = events.closed() => {
                gate.cancel();
                None
            }
            () = crate::agent::cancelled(ctx.cancel.as_ref()) => {
                gate.cancel();
                Some(PlanVerdict::reject("the review was cancelled"))
            }
        };

        Ok(match verdict {
            Some(verdict) if verdict.approved => {
                self.plan_mode.store(false, Ordering::SeqCst);
                if verdict.feedback.trim().is_empty() {
                    ToolOutput::ok("Plan approved — proceed to execute it.")
                } else {
                    ToolOutput::ok(format!(
                        "Plan approved — proceed to execute it. The reviewer added: {}",
                        verdict.feedback.trim()
                    ))
                }
            }
            Some(verdict) => {
                let feedback = if verdict.feedback.trim().is_empty() {
                    "the plan was not accepted".to_string()
                } else {
                    verdict.feedback
                };
                ToolOutput::error(format!(
                    "plan rejected: {feedback}. Revise the plan (you are still in plan mode) \
                     and call exit_plan again."
                ))
            }
            None => ToolOutput::error("plan review ended without a verdict; still in plan mode"),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tokio::sync::mpsc;

    use super::*;

    /// Temp project dir removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("wizard-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn flag(on: bool) -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(on))
    }

    #[tokio::test]
    async fn errors_outside_plan_mode() {
        let tmp = TempDir::new();
        let tool = ExitPlanTool::new(flag(false), flag(false));
        let out = tool
            .execute(json!({ "plan": "# p" }), &ToolContext::new(&tmp.0))
            .await
            .expect("executes");
        assert!(out.is_error);
        assert!(out.content.contains("not in plan mode"), "{}", out.content);
        assert!(!tmp.0.join(PLAN_FILE).exists(), "no plan file written");
    }

    #[tokio::test]
    async fn errors_without_an_event_channel() {
        let tmp = TempDir::new();
        let tool = ExitPlanTool::new(flag(true), flag(false));
        let out = tool
            .execute(json!({ "plan": "# p" }), &ToolContext::new(&tmp.0))
            .await
            .expect("executes");
        assert!(out.is_error);
        assert!(out.content.contains("unavailable"), "{}", out.content);
    }

    #[tokio::test]
    async fn approval_writes_plan_and_clears_the_flag() {
        let tmp = TempDir::new();
        let plan_mode = flag(true);
        let tool = ExitPlanTool::new(Arc::clone(&plan_mode), flag(false));
        let (tx, mut rx) = mpsc::channel(8);
        let ctx = ToolContext::new(&tmp.0).with_events(tx);

        let reviewer = async {
            let Some(AgentEvent::PlanReady { plan, gate }) = rx.recv().await else {
                panic!("expected PlanReady");
            };
            assert_eq!(plan, "# the plan");
            assert!(gate.answer(PlanVerdict::approve()), "verdict sent");
        };
        let (out, ()) = tokio::join!(
            tool.execute(json!({ "plan": "# the plan" }), &ctx),
            reviewer
        );
        let out = out.expect("executes");

        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("Plan approved"), "{}", out.content);
        assert!(!plan_mode.load(Ordering::SeqCst), "plan mode cleared");
        let saved = std::fs::read_to_string(tmp.0.join(PLAN_FILE)).expect("plan persisted");
        assert_eq!(saved, "# the plan");
    }

    #[tokio::test]
    async fn approval_passes_the_reviewers_word_on() {
        let tmp = TempDir::new();
        let tool = ExitPlanTool::new(flag(true), flag(false));
        let (tx, mut rx) = mpsc::channel(8);
        let ctx = ToolContext::new(&tmp.0).with_events(tx);

        let reviewer = async {
            let Some(AgentEvent::PlanReady { gate, .. }) = rx.recv().await else {
                panic!("expected PlanReady");
            };
            let verdict = PlanVerdict {
                approved: true,
                feedback: " mind the tests ".to_string(),
            };
            assert!(gate.answer(verdict), "verdict sent");
        };
        let (out, ()) = tokio::join!(tool.execute(json!({ "plan": "# p" }), &ctx), reviewer);
        let out = out.expect("executes");

        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.ends_with("The reviewer added: mind the tests"),
            "{}",
            out.content
        );
    }

    #[tokio::test]
    async fn rejection_keeps_plan_mode_and_returns_feedback() {
        let tmp = TempDir::new();
        let plan_mode = flag(true);
        let tool = ExitPlanTool::new(Arc::clone(&plan_mode), flag(false));
        let (tx, mut rx) = mpsc::channel(8);
        let ctx = ToolContext::new(&tmp.0).with_events(tx);

        let reviewer = async {
            let Some(AgentEvent::PlanReady { gate, .. }) = rx.recv().await else {
                panic!("expected PlanReady");
            };
            assert!(
                gate.answer(PlanVerdict::reject("cover the error path too")),
                "verdict sent"
            );
        };
        let (out, ()) = tokio::join!(tool.execute(json!({ "plan": "# p" }), &ctx), reviewer);
        let out = out.expect("executes");

        assert!(out.is_error);
        assert!(
            out.content.contains("cover the error path too"),
            "{}",
            out.content
        );
        assert!(plan_mode.load(Ordering::SeqCst), "plan mode stays on");
    }

    #[tokio::test]
    async fn dropped_verdict_keeps_plan_mode() {
        let tmp = TempDir::new();
        let plan_mode = flag(true);
        let tool = ExitPlanTool::new(Arc::clone(&plan_mode), flag(false));
        let (tx, mut rx) = mpsc::channel(8);
        let ctx = ToolContext::new(&tmp.0).with_events(tx);

        let reviewer = async {
            let Some(AgentEvent::PlanReady { gate, .. }) = rx.recv().await else {
                panic!("expected PlanReady");
            };
            // Surface went away without answering: claiming and dropping the
            // channel is what a review modal being torn down does.
            drop(gate.claim());
        };
        let (out, ()) = tokio::join!(tool.execute(json!({ "plan": "# p" }), &ctx), reviewer);
        let out = out.expect("executes");
        assert!(out.is_error);
        assert!(plan_mode.load(Ordering::SeqCst), "plan mode stays on");
    }

    #[tokio::test]
    async fn cancelling_the_turn_ends_an_unanswered_review() {
        let tmp = TempDir::new();
        let plan_mode = flag(true);
        let tool = ExitPlanTool::new(Arc::clone(&plan_mode), flag(false));
        let (tx, mut rx) = mpsc::channel(8);
        let cancel = crate::agent::CancelHandle::default();
        let ctx = ToolContext::new(&tmp.0)
            .with_events(tx)
            .with_cancel(cancel.clone());

        // the surface takes the question and never answers it
        let silent = async {
            let Some(AgentEvent::PlanReady { gate, .. }) = rx.recv().await else {
                panic!("expected PlanReady");
            };
            cancel.cancel();
            gate
        };
        let (out, _gate) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(tool.execute(json!({ "plan": "# p" }), &ctx), silent)
        })
        .await
        .expect("the review ends when the turn is cancelled");
        let out = out.expect("executes");
        assert!(out.is_error);
        assert!(
            out.content.contains("the review was cancelled"),
            "{}",
            out.content
        );
        assert!(plan_mode.load(Ordering::SeqCst), "plan mode stays on");
    }

    #[tokio::test]
    async fn omakase_auto_approves_without_a_review_round_trip() {
        let tmp = TempDir::new();
        let plan_mode = flag(true);
        let omakase = flag(true);
        let tool = ExitPlanTool::new(Arc::clone(&plan_mode), Arc::clone(&omakase));
        let (tx, mut rx) = mpsc::channel(8);
        let ctx = ToolContext::new(&tmp.0).with_events(tx);

        // The surface only observes an informational OmakaseProceeding event;
        // it never sends a verdict.
        let observer = async {
            match rx.recv().await {
                Some(AgentEvent::OmakaseProceeding { plan }) => plan,
                other => panic!("expected OmakaseProceeding, got {other:?}"),
            }
        };
        let (out, announced) = tokio::join!(
            tool.execute(json!({ "plan": "# chef plan" }), &ctx),
            observer
        );
        let out = out.expect("executes");

        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("Omakase"), "{}", out.content);
        assert_eq!(announced, "# chef plan");
        assert!(!plan_mode.load(Ordering::SeqCst), "plan mode cleared");
        assert!(!omakase.load(Ordering::SeqCst), "omakase cleared");
        let saved = std::fs::read_to_string(tmp.0.join(PLAN_FILE)).expect("plan persisted");
        assert_eq!(saved, "# chef plan");
    }

    #[tokio::test]
    async fn omakase_proceeds_even_without_a_surface() {
        // Subagent context (no event channel): omakase still auto-approves
        // rather than refusing, because the chef needs no human gate.
        let tmp = TempDir::new();
        let plan_mode = flag(true);
        let omakase = flag(true);
        let tool = ExitPlanTool::new(Arc::clone(&plan_mode), Arc::clone(&omakase));
        let out = tool
            .execute(json!({ "plan": "# p" }), &ToolContext::new(&tmp.0))
            .await
            .expect("executes");
        assert!(!out.is_error, "{}", out.content);
        assert!(!plan_mode.load(Ordering::SeqCst), "plan mode cleared");
    }
}
