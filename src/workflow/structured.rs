//! `StructuredOutput`: how a workflow agent returns an object.
//!
//! A subagent started with `agent(prompt, {schema})` gets this tool, whose
//! input schema is the script's. The call is validated here, so a wrong shape
//! comes back to the model as an error it can correct. After
//! [`MAX_RETRIES`] bad calls the tool stops accepting and the run fails with
//! the last complaint, as Claude's `MAX_STRUCTURED_OUTPUT_RETRIES` does.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use serde_json::Value;

use crate::tools::{Tool, ToolAccess, ToolContext, ToolError, ToolOutput};

pub const TOOL_NAME: &str = "StructuredOutput";

/// Bad calls tolerated before the agent is given up on.
pub const MAX_RETRIES: u32 = 5;

/// What one agent run produced through the tool.
#[derive(Default)]
pub struct Captured {
    value: Mutex<Option<Value>>,
    rejections: AtomicU32,
    last_error: Mutex<Option<String>>,
}

impl Captured {
    pub fn value(&self) -> Option<Value> {
        self.value.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn rejections(&self) -> u32 {
        self.rejections.load(Ordering::SeqCst)
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

pub struct StructuredOutputTool {
    schema: Value,
    captured: std::sync::Arc<Captured>,
}

impl StructuredOutputTool {
    pub fn new(schema: Value) -> (Self, std::sync::Arc<Captured>) {
        let captured = std::sync::Arc::new(Captured::default());
        (
            Self {
                schema,
                captured: std::sync::Arc::clone(&captured),
            },
            captured,
        )
    }
}

#[async_trait]
impl Tool for StructuredOutputTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn description(&self) -> &str {
        "Return your final answer. Call this exactly once with the result in the shape the schema asks for, then end your turn. If the call is rejected, read the error and call it again with a corrected shape."
    }

    fn parameters(&self) -> Value {
        self.schema.clone()
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        if self.captured.rejections() >= MAX_RETRIES {
            return Ok(ToolOutput::error(
                "Too many invalid attempts. Stop and end your turn.",
            ));
        }
        match super::schema::validate(&self.schema, &args) {
            Ok(()) => {
                *self
                    .captured
                    .value
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(args);
                Ok(ToolOutput::ok(
                    "Structured output provided successfully. End your turn now without calling more tools.",
                ))
            }
            Err(why) => {
                self.captured.rejections.fetch_add(1, Ordering::SeqCst);
                *self
                    .captured
                    .last_error
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(why.clone());
                Ok(ToolOutput::error(format!(
                    "Output does not match the required schema: {why}. Call {TOOL_NAME} again with a corrected shape."
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn tool() -> (StructuredOutputTool, std::sync::Arc<Captured>) {
        StructuredOutputTool::new(json!({
            "type": "object",
            "properties": {"n": {"type": "integer"}},
            "required": ["n"]
        }))
    }

    #[tokio::test]
    async fn a_valid_call_is_captured() {
        let (tool, captured) = tool();
        let out = tool
            .execute(json!({"n": 3}), &ToolContext::new("."))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(captured.value(), Some(json!({"n": 3})));
    }

    #[tokio::test]
    async fn an_invalid_call_is_an_error_the_model_can_read_and_is_counted() {
        let (tool, captured) = tool();
        let out = tool
            .execute(json!({"n": "x"}), &ToolContext::new("."))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(
            out.content.contains("expected integer, got string"),
            "{}",
            out.content
        );
        assert_eq!(captured.rejections(), 1);
        assert!(captured.value().is_none());
    }

    #[tokio::test]
    async fn the_tool_stops_taking_calls_after_the_retry_cap() {
        let (tool, captured) = tool();
        for _ in 0..MAX_RETRIES {
            tool.execute(json!({}), &ToolContext::new("."))
                .await
                .unwrap();
        }
        let out = tool
            .execute(json!({"n": 1}), &ToolContext::new("."))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(captured.value().is_none());
        assert!(
            captured
                .last_error()
                .unwrap()
                .contains("missing required property")
        );
    }
}
