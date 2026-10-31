use std::time::Duration;

use serde_json::json;

use super::*;
use crate::agent::subagent::{SubagentBinding, builtin_configs};
use crate::llm::{CacheTokens, ChatChunk, ChatMessage, ChatRequest, ChatStream, Role, ToolCall};

/// Answers `ans(<task>)`, or calls StructuredOutput when the task asks for it.
struct Brain;

fn chunk(message: ChatMessage) -> ChatChunk {
    ChatChunk {
        message: Some(message),
        images: Vec::new(),
        thinking: false,
        done: true,
        done_reason: None,
        eval_count: None,
        prompt_eval_count: None,
        cache: CacheTokens::NONE,
        reasoning_eval_count: None,
        billed_ticks: None,
    }
}

#[async_trait]
impl LlmProvider for Brain {
    async fn health(&self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn supports_native_tools(&self, _model: &str) -> anyhow::Result<bool> {
        Ok(true)
    }
    async fn list_models(&self) -> anyhow::Result<Vec<String>> {
        Ok(Vec::new())
    }
    async fn chat_stream(&self, request: ChatRequest) -> anyhow::Result<ChatStream> {
        let answered = request.messages.iter().any(|m| m.role == Role::Tool);
        let task = request
            .messages
            .iter()
            .find(|m| m.role == Role::User)
            .map(ChatMessage::text)
            .unwrap_or_default();
        let task = task
            .split_once("computed task text follows:\n")
            .map_or(task.clone(), |(_, rest)| rest.to_string());
        let task = task
            .split("\n\nCRITICAL")
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        let message = if task.contains("STRUCT") && !answered {
            let mut message = ChatMessage::assistant("");
            message.push_tool_call(ToolCall::new("StructuredOutput", json!({"n": 7})));
            message
        } else if answered {
            ChatMessage::assistant("finished")
        } else {
            ChatMessage::assistant(format!("ans({task})"))
        };
        Ok(Box::pin(futures_util::stream::iter(vec![Ok(chunk(
            message,
        ))])))
    }
    fn label(&self) -> String {
        "brain:test".into()
    }
}

fn tool() -> WorkflowTool {
    let hooks = Arc::new(HookEngine::new(
        Vec::new(),
        std::env::temp_dir(),
        "test".into(),
    ));
    WorkflowTool::new(
        builtin_configs(),
        Arc::new(Brain),
        Arc::new(ToolRegistry::with_native_tools()),
        hooks,
        Arc::new(SubagentBinding::default()),
    )
}

fn ctx() -> ToolContext {
    ToolContext::new(std::env::temp_dir())
}

const FAN: &str = "export const meta = {name: 'fan', description: 'fan out two'}\nphase('Go')\nconst rs = await parallel(['a', 'b'].map(x => () => agent('say ' + x, {label: x})))\nreturn rs\n";

#[test]
fn the_input_schema_takes_claudes_fields_and_requires_none() {
    let schema = tool().parameters();
    let props: Vec<&str> = schema["properties"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    for want in [
        "script",
        "name",
        "scriptPath",
        "args",
        "resumeFromRunId",
        "title",
        "description",
    ] {
        assert!(props.contains(&want), "missing {want}: {props:?}");
    }
    assert!(schema.get("required").is_none());
    assert_eq!(tool().name(), "Workflow");
    assert!(tool().deferred_by_default());
}

#[tokio::test]
async fn a_run_where_nothing_can_be_told_later_waits_and_returns_the_result() {
    let out = tool()
        .execute(json!({"script": FAN}), &ctx())
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.starts_with("Run ID: wf_"), "{}", out.content);
    assert!(out.content.contains("completed in"), "{}", out.content);
    assert!(out.content.contains("ans(say a)"), "{}", out.content);
    assert!(out.content.contains("ans(say b)"), "{}", out.content);
}

#[tokio::test]
async fn a_schema_agent_returns_the_validated_object() {
    let script = "export const meta = {name: 's', description: 'structured'}\nconst r = await agent('STRUCT please', {schema: {type: 'object', properties: {n: {type: 'integer'}}, required: ['n']}})\nreturn r.n + 1\n";
    let out = tool()
        .execute(json!({"script": script}), &ctx())
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("\n8\n"), "{}", out.content);
}

#[tokio::test]
async fn an_interactive_session_gets_the_launch_text_and_a_task_that_finishes() {
    let mut ctx = ctx();
    ctx.command_dispatch = CommandDispatch::All;
    let out = tool().execute(json!({"script": FAN}), &ctx).await.unwrap();
    assert!(!out.is_error, "{}", out.content);
    for want in [
        "Workflow launched in background. Task ID: 1",
        "Summary: fan out two",
        "Script file: ",
        "Run ID: wf_",
        "resumeFromRunId",
        "You will be notified when it completes. Use /workflows to watch live progress.",
    ] {
        assert!(out.content.contains(want), "no {want:?} in {}", out.content);
    }
    let mut waited = 0;
    let finished = loop {
        let done = ctx.tasks.drain_completed();
        if !done.is_empty() {
            break done;
        }
        waited += 1;
        assert!(waited < 100, "the workflow never finished");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(finished[0].command.starts_with("workflow fan"));
    assert!(
        finished[0].tail.contains("ans(say b)"),
        "{}",
        finished[0].tail
    );
}

#[tokio::test]
async fn bad_input_is_an_error_result_with_claudes_wording() {
    let no_input = tool().execute(json!({}), &ctx()).await.unwrap();
    assert!(no_input.is_error);
    assert_eq!(no_input.content, "Must provide script, name, or scriptPath");

    let missing = tool()
        .execute(json!({"name": "does-not-exist"}), &ctx())
        .await
        .unwrap();
    assert!(
        missing
            .content
            .starts_with("Workflow \"does-not-exist\" not found. Available:")
    );

    let no_meta = tool()
        .execute(json!({"script": "return 1"}), &ctx())
        .await
        .unwrap();
    assert!(
        no_meta.content.contains("must be the FIRST statement"),
        "{}",
        no_meta.content
    );

    let clock = tool()
        .execute(
            json!({"script": "export const meta = {name: 'c', description: 'd'}\nreturn Date.now()"}),
            &ctx(),
        )
        .await
        .unwrap();
    assert!(
        clock.content.contains("must be deterministic"),
        "{}",
        clock.content
    );
}

#[test]
fn the_environment_variable_turns_workflows_off() {
    let env = |value: &'static str| {
        move |k: &str| (k == "CLAUDE_CODE_DISABLE_WORKFLOWS").then(|| value.to_string())
    };
    assert!(disabled_by_env(&env("1")));
    assert!(disabled_by_env(&env("true")));
    assert!(!disabled_by_env(&env("0")));
    assert!(!disabled_by_env(&env("")));
    assert!(!disabled_by_env(&|_| None));
}

#[test]
fn the_size_guideline_follows_the_setting() {
    let default = size_guideline(None).unwrap();
    assert!(default.starts_with("This session has the default workflow size guideline: medium"));
    assert!(default.contains("under 10 agents") && default.contains("workflowSizeGuideline"));
    let small = size_guideline(Some("small")).unwrap();
    assert!(small.starts_with("A workflow size guideline is configured for this session: small"));
    assert!(small.contains("under 5 agents") && !small.contains("Set workflowSizeGuideline"));
    assert!(
        size_guideline(Some("large"))
            .unwrap()
            .contains("under 50 agents")
    );
    assert_eq!(size_guideline(Some("unrestricted")), None);

    let described = tool()
        .with_size_guideline(Some(small.clone()))
        .description()
        .to_string();
    assert!(described.ends_with(&small));
    assert!(
        !tool()
            .with_size_guideline(None)
            .description()
            .contains("guideline")
    );
}
