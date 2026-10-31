//! `advisor`: ask a second model for guidance mid-task. See [`crate::advisor`].

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Tool, ToolAccess, ToolContext, ToolError, ToolOutput};

pub fn tools() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(AdvisorTool)]
}

pub struct AdvisorTool;

#[async_trait]
impl Tool for AdvisorTool {
    fn name(&self) -> &str {
        "advisor"
    }

    fn description(&self) -> &str {
        "Ask a second, usually stronger, model for guidance before you commit to an approach, \
         when you are stuck, or before you call a hard task done. The user chose it with \
         /advisor <model>. It sees only the summary you write in `context` (not the \
         conversation, files or tools), makes one plain completion and returns advice; it never \
         acts. Write `context` as a short brief: the goal, what you tried, what you know, what \
         failed. If no advisor is set the call says how to set one."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": { "type": "string", "description": "What you want advice on." },
                "context": {
                    "type": "string",
                    "description": "Summary of the task and where it stands: goal, constraints, what you tried, what failed, the relevant code or output. The advisor sees nothing else."
                }
            },
            "required": ["question"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    fn deferred_by_default(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let Some(question) = args.get("question").and_then(Value::as_str) else {
            return Ok(super::compat::invalid(self.name(), "question is required"));
        };
        let context = args.get("context").and_then(Value::as_str).unwrap_or("");
        Ok(
            match crate::advisor::consult_configured(question, context, ctx.usage.as_deref()).await
            {
                Ok(text) => ToolOutput::ok(text),
                Err(err) => ToolOutput::error(err),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn without_a_setting_the_call_says_how_to_set_one() {
        // SAFETY of the env read: the test only needs ENV unset, which it is
        // unless the developer exported it; the settings file is the test dir's.
        if crate::advisor::configured().is_some() {
            return;
        }
        let out = AdvisorTool
            .execute(json!({"question": "q"}), &ToolContext::new("."))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("/advisor <model>"), "{}", out.content);
    }

    #[tokio::test]
    async fn the_question_is_required() {
        let out = AdvisorTool
            .execute(json!({}), &ToolContext::new("."))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.starts_with("Tool advisor input is invalid"));
    }

    #[test]
    fn it_is_deferred_and_read_only() {
        assert!(AdvisorTool.deferred_by_default());
        assert!(matches!(AdvisorTool.access(), ToolAccess::ReadOnly));
    }
}
