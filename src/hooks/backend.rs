//! The services behind `prompt`, `agent` and `mcp_tool` hooks, built from
//! the same model client and tool list the session itself uses.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::Value;

use super::engine::HookBackend;
use crate::llm::provider::LlmProvider;
use crate::llm::{ChatMessage, ChatOptions, ChatRequest};
use crate::tools::{Tool, ToolContext};

/// Model client plus the connected MCP tools.
pub struct SessionBackend {
    client: Arc<dyn LlmProvider>,
    model: String,
    tools: HashMap<String, Arc<dyn Tool>>,
    root: PathBuf,
}

impl SessionBackend {
    pub fn new(
        client: Arc<dyn LlmProvider>,
        model: String,
        tools: Vec<Arc<dyn Tool>>,
        root: PathBuf,
    ) -> Self {
        let tools = tools
            .into_iter()
            .map(|tool| (tool.name().to_string(), tool))
            .collect();
        Self {
            client,
            model,
            tools,
            root,
        }
    }
}

#[async_trait]
impl HookBackend for SessionBackend {
    async fn ask_model(&self, prompt: &str, _model: Option<&str>) -> Result<String, String> {
        let request = ChatRequest {
            model: self.model.clone(),
            messages: vec![ChatMessage::user(prompt.to_string())],
            tools: Vec::new(),
            stream: true,
            options: Some(ChatOptions {
                temperature: Some(0.0),
                num_ctx: None,
                reasoning_effort: None,
            }),
        };
        let mut stream = self
            .client
            .chat_stream(request)
            .await
            .map_err(|err| format!("model call failed: {err}"))?;
        let mut answer = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|err| format!("model stream failed: {err}"))?;
            if let Some(message) = chunk.message
                && !chunk.thinking
            {
                answer.push_str(&message.text());
            }
            if chunk.done {
                break;
            }
        }
        Ok(answer)
    }

    /// Wizard runs an `agent` hook as a prompt hook: one model call with no
    /// tool access. A hook that needs to look at the repository should shell
    /// out in a `command` hook instead.
    async fn run_agent(&self, prompt: &str, model: Option<&str>) -> Result<String, String> {
        self.ask_model(prompt, model).await
    }

    async fn call_mcp(
        &self,
        server: &str,
        tool: &str,
        input: Value,
    ) -> Result<(String, bool), String> {
        let namespaced = format!("{server}__{tool}");
        let tool = self
            .tools
            .get(&namespaced)
            .or_else(|| self.tools.get(tool))
            .ok_or_else(|| format!("no MCP tool {server}/{tool} is connected"))?;
        let ctx = ToolContext::new(&self.root);
        match tool.execute(input, &ctx).await {
            Ok(output) => Ok((output.content, output.is_error)),
            Err(err) => Err(err.to_string()),
        }
    }
}
