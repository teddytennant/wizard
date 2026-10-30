//! `SendMessage` and `ListAgents`.
//!
//! Claude Code's reach across sessions, teammates and a cloud is not here;
//! Wizard's agents are the main loop and the subagents it started. So a
//! message goes to `main` (from a subagent), or to a background subagent by
//! `agent:<id>` or name. It lands in the receiver's inbox and is read at the
//! top of its next step, like a finished background task, so it does not
//! interrupt a model call in flight.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{invalid, str_arg};
use crate::tools::{Tool, ToolAccess, ToolContext, ToolError, ToolOutput};

pub fn tools() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(SendMessageTool), Arc::new(ListAgentsTool)]
}

const NOT_REACHABLE_HINT: &str = "Use ListAgents to see everyone you can message.";

/// `SendMessage`.
pub struct SendMessageTool;

/// The id of the running background subagent `to` names.
fn resolve(to: &str, ctx: &ToolContext) -> Option<u32> {
    let to = to.trim();
    let bare = to
        .strip_prefix("agent:")
        .or_else(|| to.strip_prefix('#'))
        .unwrap_or(to);
    let snapshots = ctx.subagents.list();
    let mut running = snapshots.iter().filter(|s| s.result.is_none());
    match bare.parse::<u32>() {
        Ok(id) => running.find(|s| s.id == id).map(|s| s.id),
        Err(_) => running.rfind(|s| s.name == bare).map(|s| s.id),
    }
}

#[async_trait]
impl Tool for SendMessageTool {
    fn name(&self) -> &str {
        "SendMessage"
    }

    fn description(&self) -> &str {
        "Send a message to another agent in this session: \"main\" from a subagent, or a \
         running background subagent by `agent:<id>` or name (ListAgents shows them). It is \
         read at the recipient's next step. Make the first line say what the message is about."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "to": { "type": "string", "description": "Recipient: \"main\", or a background subagent as agent:<id> or its name" },
                "summary": { "type": "string", "description": "A 5-10 word label for your own transcript. Not sent." },
                "message": { "type": "string", "description": "Plain text message content." },
                "notify_when_idle": { "type": "boolean", "description": "Accepted and ignored." }
            },
            "required": ["to", "message"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    fn deferred_by_default(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let (Some(to), Some(message)) = (str_arg(&args, "to"), str_arg(&args, "message")) else {
            return Ok(invalid(self.name(), "to and message are required"));
        };
        if to.contains(['\n', '\r']) || to.chars().count() > 300 {
            return Ok(invalid(self.name(), "to must be one short line"));
        }

        if to.trim() == "main" {
            return Ok(match &ctx.parent_inbox {
                Some(inbox) => {
                    inbox.push("subagent", message);
                    ToolOutput::ok("Message sent to main.")
                }
                // oracle: the real tool's answer, byte for byte
                None => ToolOutput::ok(
                    "{\"success\":false,\"message\":\"You are the main conversation \u{2014} \\\"main\\\" addresses you. Send to a named agent instead.\"}",
                ),
            });
        }
        let Some(id) = resolve(to, ctx) else {
            return Ok(ToolOutput::ok(format!(
                "No agent named '{to}' is reachable.\n{NOT_REACHABLE_HINT}"
            )));
        };
        let Some(inbox) = ctx.subagents.inbox_of(id) else {
            return Ok(ToolOutput::ok(format!(
                "No agent named '{to}' is reachable.\n{NOT_REACHABLE_HINT}"
            )));
        };
        inbox.push("main", message);
        Ok(ToolOutput::ok(format!(
            "Message sent to agent:{id}. It reads it at its next step."
        )))
    }
}

/// `ListAgents`.
pub struct ListAgentsTool;

#[async_trait]
impl Tool for ListAgentsTool {
    fn name(&self) -> &str {
        "ListAgents"
    }

    fn description(&self) -> &str {
        "List the agents SendMessage can reach: running background subagents of this session, \
         and main when you are a subagent."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "channel": { "type": "string", "description": "Not available in this build; leave unset." },
                "q": { "type": "string", "description": "Not available in this build; leave unset." }
            }
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    fn deferred_by_default(&self) -> bool {
        true
    }

    async fn execute(&self, _args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let mut rows: Vec<String> = Vec::new();
        if ctx.parent_inbox.is_some() {
            rows.push("main (the agent that started you)".to_string());
        }
        for snapshot in ctx.subagents.list().iter().filter(|s| s.result.is_none()) {
            let task: String = snapshot.task.chars().take(80).collect();
            rows.push(format!(
                "agent:{} {} (running): {task}",
                snapshot.id, snapshot.name
            ));
        }
        if rows.is_empty() {
            return Ok(ToolOutput::ok("No reachable agents."));
        }
        Ok(ToolOutput::ok(rows.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::subagent_tasks::Inbox;

    fn ctx() -> ToolContext {
        ToolContext::new(std::env::temp_dir())
    }

    async fn call(tool: &dyn Tool, ctx: &ToolContext, args: Value) -> ToolOutput {
        tool.execute(args, ctx).await.unwrap()
    }

    #[tokio::test]
    async fn nobody_to_message_reads_like_claude_code() {
        let ctx = ctx();
        let listed = call(&ListAgentsTool, &ctx, json!({})).await;
        assert_eq!(listed.content, "No reachable agents.");
        let sent = call(
            &SendMessageTool,
            &ctx,
            json!({ "to": "nobody", "message": "hi" }),
        )
        .await;
        assert!(!sent.is_error);
        assert_eq!(
            sent.content,
            "No agent named 'nobody' is reachable.\nUse ListAgents to see everyone you can message."
        );
    }

    #[tokio::test]
    async fn a_message_reaches_a_running_subagent_by_id_or_name() {
        let ctx = ctx();
        let id = ctx.subagents.reserve("reviewer", "review the diff");
        let inbox = ctx.subagents.inbox_of(id).unwrap();

        let listed = call(&ListAgentsTool, &ctx, json!({})).await;
        assert_eq!(
            listed.content,
            "agent:1 reviewer (running): review the diff"
        );

        call(
            &SendMessageTool,
            &ctx,
            json!({ "to": "agent:1", "message": "also check tests" }),
        )
        .await;
        call(
            &SendMessageTool,
            &ctx,
            json!({ "to": "reviewer", "message": "and docs" }),
        )
        .await;
        let mail = inbox.take();
        assert_eq!(mail.len(), 2);
        assert_eq!(mail[0].from, "main");
        assert_eq!(mail[0].text, "also check tests");
        assert_eq!(mail[1].text, "and docs");
        assert!(inbox.take().is_empty(), "each message is read once");
    }

    #[tokio::test]
    async fn a_finished_subagent_is_not_reachable() {
        let ctx = ctx();
        let id = ctx.subagents.reserve("reviewer", "t");
        ctx.subagents.kill(id);
        let out = call(&SendMessageTool, &ctx, json!({ "to": "1", "message": "x" })).await;
        assert!(out.content.starts_with("No agent named '1' is reachable."));
    }

    #[tokio::test]
    async fn a_subagent_can_message_main_and_main_cannot() {
        let mut sub = ctx();
        let parent = Arc::new(Inbox::default());
        sub.parent_inbox = Some(Arc::clone(&parent));
        let out = call(
            &SendMessageTool,
            &sub,
            json!({ "to": "main", "message": "done early" }),
        )
        .await;
        assert_eq!(out.content, "Message sent to main.");
        assert_eq!(parent.take()[0].text, "done early");
        assert!(
            call(&ListAgentsTool, &sub, json!({}))
                .await
                .content
                .starts_with("main")
        );

        let main = ctx();
        let out = call(
            &SendMessageTool,
            &main,
            json!({ "to": "main", "message": "x" }),
        )
        .await;
        assert_eq!(
            out.content,
            "{\"success\":false,\"message\":\"You are the main conversation \u{2014} \\\"main\\\" addresses you. Send to a named agent instead.\"}"
        );
    }

    #[tokio::test]
    async fn to_must_be_one_line() {
        let out = call(
            &SendMessageTool,
            &ctx(),
            json!({ "to": "a\nb", "message": "x" }),
        )
        .await;
        assert!(out.is_error);
    }
}
