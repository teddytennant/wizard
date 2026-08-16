use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use futures_util::stream;

use super::*;
use crate::config::StepBudget;
use crate::headless::rollback_failed_cycle;
use crate::hooks::{HookDef, HookEvent};
use crate::llm::{CacheTokens, ChatChunk, ChatStream};
use crate::tools::ToolOutput;

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

/// Provider that replays canned chunk sequences and records the requests
/// it received, for exercising the agent loop without a server.
#[derive(Debug)]
struct ScriptedProvider {
    responses: Mutex<VecDeque<Vec<ChatChunk>>>,
    requests: Mutex<Vec<ChatRequest>>,
    /// Reported context window (None = unknown, like a local model).
    context_window: Option<u32>,
    /// Upcoming chat_stream calls that fail with a transient transport
    /// error before the scripted responses resume.
    fail: Mutex<u32>,
}

impl ScriptedProvider {
    fn new(responses: Vec<Vec<ChatChunk>>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(Vec::new()),
            context_window: None,
            fail: Mutex::new(0),
        })
    }

    fn with_context_window(responses: Vec<Vec<ChatChunk>>, window: u32) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(Vec::new()),
            context_window: Some(window),
            fail: Mutex::new(0),
        })
    }

    fn flaky(failures: u32, responses: Vec<Vec<ChatChunk>>) -> Arc<Self> {
        let provider = Self::new(responses);
        *provider.fail.lock().unwrap() = failures;
        provider
    }
}

#[async_trait::async_trait]
impl LlmProvider for ScriptedProvider {
    async fn health(&self) -> Result<()> {
        Ok(())
    }

    async fn supports_native_tools(&self, _model: &str) -> Result<bool> {
        Ok(true)
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    async fn chat_stream(&self, request: ChatRequest) -> Result<ChatStream> {
        self.requests.lock().unwrap().push(request);
        {
            let mut fail = self.fail.lock().unwrap();
            if *fail > 0 {
                *fail -= 1;
                return Err(crate::llm::ProviderError::transport("scripted flake").into());
            }
        }
        let chunks = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("scripted response available");
        Ok(futures_util::StreamExt::boxed(stream::iter(
            chunks.into_iter().map(Ok),
        )))
    }

    async fn context_window(&self, _model: &str) -> Option<u32> {
        self.context_window
    }

    fn label(&self) -> String {
        "scripted:test".to_string()
    }
}

/// Provider whose every call fails with a transient error, to exercise the
/// endpoint circuit breaker's fail-fast.
#[derive(Debug)]
struct FailingProvider {
    calls: Mutex<u32>,
}

#[async_trait::async_trait]
impl LlmProvider for FailingProvider {
    async fn health(&self) -> Result<()> {
        Ok(())
    }
    async fn supports_native_tools(&self, _model: &str) -> Result<bool> {
        Ok(true)
    }
    async fn list_models(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
    async fn chat_stream(&self, _request: ChatRequest) -> Result<ChatStream> {
        *self.calls.lock().unwrap() += 1;
        // A transport failure (status None) is transient, so the retry loop
        // keeps trying — until the breaker trips.
        Err(crate::llm::ProviderError::transport("simulated outage").into())
    }
    async fn context_window(&self, _model: &str) -> Option<u32> {
        None
    }
    fn label(&self) -> String {
        "failing:test".to_string()
    }
}

/// An agent talking to `client`, in `tmp`, under `config`.
fn agent_with(tmp: &TempDir, client: Arc<dyn LlmProvider>, config: Config) -> Agent {
    let session = Session::create(&tmp.0).expect("create session");
    let hooks = Arc::new(HookEngine::new(
        Vec::new(),
        tmp.0.clone(),
        session.id.clone(),
    ));
    let mut agent = Agent::new(
        client,
        ToolRegistry::new(),
        config,
        Vec::new(),
        tmp.0.clone(),
        session,
        true,
        hooks,
    )
    .expect("build agent");
    agent.set_usage_log(Some(tmp.0.join("usage.jsonl")));
    agent
}

/// **The complaint, at the endpoint end.** A standing mission must sleep
/// through a provider outage, not die of one.
///
/// Continuous mode has no per-turn attempt cap, so the endpoint breaker is the
/// only thing shaping the climb — and it used to end it. Eight consecutive
/// failures is about ten minutes on the default ladder, so a provider blip
/// longer than a coffee break ended a mission that had been running for hours,
/// with nobody around to restart it. Now the turn waits the outage out.
///
/// It is still bounded: patience runs out (some permanent failures are
/// indistinguishable from an outage, because an unrecognized error is
/// *defaulted* to transient) and the turn then ends as a clean circuit breaker
/// rather than a hard error, so the cycle is rolled back properly. Time is
/// paused, so half a day of waiting costs the test nothing.
#[tokio::test(start_paused = true)]
async fn a_continuous_run_waits_a_down_provider_out_rather_than_dying_of_it() {
    let tmp = TempDir::new();
    let provider = Arc::new(FailingProvider {
        calls: Mutex::new(0),
    });
    let mut agent = agent_with(
        &tmp,
        provider.clone(),
        Config {
            continuous: true,
            // Zero backoff: what this test measures is the breaker's cooldowns,
            // not the ladder's delays.
            retry_base_secs: 0,
            retry_max_secs: 0,
            ..Config::default()
        },
    );

    let started = tokio::time::Instant::now();
    let (tx, _rx) = mpsc::channel(256);
    let reason = agent
        .run_turn("do something", tx)
        .await
        .expect("the turn resolves rather than hanging");
    assert_eq!(
        reason,
        DoneReason::CircuitBreaker,
        "and it ends cleanly, so the cycle is rolled back rather than errored"
    );

    let waited = started.elapsed();
    assert!(
        waited > Duration::from_secs(60 * 60),
        "it waited hours rather than giving up in the first ten minutes: {waited:?}"
    );
    // The escalating cooldown is what makes waiting affordable: a handful of
    // dials an hour, not one every thirty seconds for half a day.
    let calls = *provider.calls.lock().unwrap();
    assert!(
        (8..100).contains(&calls),
        "{calls} calls: the threshold's worth, then one probe per widening cooldown"
    );
}

/// The same outage on a watched turn ends it promptly instead, on the retry
/// budget, with the provider's own message. Somebody is looking at a spinner
/// and would rather be told.
#[tokio::test(start_paused = true)]
async fn an_interactive_turn_surfaces_a_down_provider_instead_of_waiting() {
    let tmp = TempDir::new();
    let provider = Arc::new(FailingProvider {
        calls: Mutex::new(0),
    });
    let mut agent = agent_with(
        &tmp,
        provider.clone(),
        Config {
            continuous: false,
            ..Config::default()
        },
    );

    let (tx, _rx) = mpsc::channel(256);
    let err = agent
        .run_turn("do something", tx)
        .await
        .expect_err("a down provider fails a watched turn");
    assert!(format!("{err:#}").contains("simulated outage"), "{err:#}");
    assert_eq!(
        *provider.calls.lock().unwrap(),
        crate::agent::turn::RETRY_ATTEMPTS + 1,
        "the first attempt plus the budget, and no waiting on the breaker"
    );
}

/// `done: true` chunk; `content` becomes the visible message when
/// non-empty.
fn final_chunk(content: &str) -> ChatChunk {
    ChatChunk {
        message: (!content.is_empty()).then(|| ChatMessage::assistant(content)),
        images: Vec::new(),
        thinking: false,
        done: true,
        done_reason: None,
        eval_count: None,
        prompt_eval_count: None,
        cache: CacheTokens::NONE,
    }
}

/// A tiny PNG (a real magic number and some bytes behind it).
fn test_png() -> Image {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    bytes.extend_from_slice(b"a few pixels");
    Image::from_bytes(&bytes).expect("a PNG")
}

/// A live chunk carrying a generated image — what an image-capable provider
/// emits on [`ChatChunk::images`].
fn image_chunk(images: Vec<Image>) -> ChatChunk {
    ChatChunk {
        message: None,
        images,
        thinking: false,
        done: false,
        done_reason: None,
        eval_count: None,
        prompt_eval_count: None,
        cache: CacheTokens::NONE,
    }
}

/// Every [`AgentEvent::Images`] a turn emitted, flattened.
fn drain_images(rx: &mut mpsc::Receiver<AgentEvent>) -> Vec<(ImageSource, ImageRef)> {
    let mut out = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let AgentEvent::Images { source, images } = event {
            out.extend(images.into_iter().map(|image| (source.clone(), image)));
        }
    }
    out
}

/// A tool that returns `images` alongside its text.
struct ImageTool {
    images: Vec<Image>,
}

#[async_trait::async_trait]
impl crate::tools::Tool for ImageTool {
    fn name(&self) -> &str {
        "generate_image"
    }
    fn description(&self) -> &str {
        "Generate an image."
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn execute(
        &self,
        _args: Value,
        _ctx: &crate::tools::ToolContext,
    ) -> Result<ToolOutput, crate::tools::ToolError> {
        Ok(ToolOutput::ok_with_images(
            "rendered 1 image",
            self.images.clone(),
        ))
    }
}

/// One assistant message whose only tool call is `generate_image`.
fn calls_image_tool() -> ChatChunk {
    let mut assistant = ChatMessage::assistant("");
    assistant.push_tool_call(ToolCall::new("generate_image".to_string(), json!({})));
    ChatChunk {
        message: Some(assistant),
        images: Vec::new(),
        thinking: false,
        done: true,
        done_reason: None,
        eval_count: None,
        prompt_eval_count: None,
        cache: CacheTokens::NONE,
    }
}

#[tokio::test]
async fn model_generated_images_reach_history_disk_and_the_surfaces() {
    // A provider that streams text and then an image, exactly as an
    // image-capable endpoint does through `ChatChunk::images`.
    let image = test_png();
    let (mut agent, _provider, _tmp) = test_agent(vec![vec![
        ChatChunk {
            message: Some(ChatMessage::assistant("here you go")),
            ..image_chunk(Vec::new())
        },
        image_chunk(vec![image.clone()]),
        final_chunk(""),
    ]]);

    let (tx, mut rx) = mpsc::channel(64);
    agent.run_turn("draw a cat", tx).await.expect("turn ok");

    // In history, on the assistant message, as base64 — a vision model
    // needs it there on the next turn.
    let assistant = agent
        .history()
        .iter()
        .rev()
        .find(|message| message.role == Role::Assistant)
        .expect("an assistant message");
    assert_eq!(assistant.text(), "here you go");
    assert_eq!(assistant.images().len(), 1);
    assert_eq!(assistant.images()[0].b64, image.b64);
    assert_eq!(assistant.images()[0].mime, "image/png");

    // Announced to the surfaces as a path, not base64.
    let announced = drain_images(&mut rx);
    assert_eq!(announced.len(), 1);
    let (source, saved) = &announced[0];
    assert_eq!(*source, ImageSource::Assistant);
    assert_eq!(saved.mime, "image/png");
    assert_eq!(
        assistant.images()[0].path.as_ref(),
        Some(&saved.path),
        "history records the same path the surfaces were given — a replayed \
         transcript re-derives nothing"
    );

    // And on disk, under this session's image directory.
    assert_eq!(
        std::fs::read(&saved.path).expect("the image file"),
        image.decode().unwrap()
    );
    assert!(
        saved
            .path
            .starts_with(Config::images_dir().unwrap().join(&agent.session().id)),
        "images are session-scoped: {}",
        saved.path.display()
    );
}

/// A parallel batch answers on ONE message, and everything else the batch
/// produced lands after it.
///
/// Both halves are wire correctness, not tidiness. Anthropic requires all of
/// an assistant turn's results in the single message that follows it, so the
/// old message-per-result shape was a 400 on any two-call reply. OpenAI takes
/// one `tool` message per result but rejects anything interleaved between
/// them, and the images payload (a user message) used to be pushed the moment
/// the tool that produced it returned, i.e. between the two results.
#[tokio::test]
async fn a_parallel_batch_answers_on_one_message_with_images_after_it() {
    let image = test_png();
    let (mut registry, echo_calls) = recording_registry();
    registry.register(Arc::new(ImageTool {
        images: vec![image.clone()],
    }));

    let tmp = TempDir::new();
    // One reply, two calls: the image tool first, so a per-call push would
    // put its user message between the two results.
    let (mut agent, _provider) = test_agent_in(
        &tmp,
        vec![
            vec![multi_tool_chunk(&["generate_image", "echo"])],
            vec![final_chunk("done")],
        ],
        Vec::new(),
        registry,
    );

    let (tx, _rx) = mpsc::channel(64);
    agent.run_turn("make one and echo", tx).await.expect("turn");
    assert_eq!(echo_calls.lock().unwrap().len(), 1, "both calls ran");

    let history = agent.history();
    let assistant = history
        .iter()
        .position(|message| message.role == Role::Assistant && message.tool_calls().len() == 2)
        .expect("the two-call reply");
    let calls: Vec<String> = history[assistant]
        .tool_calls()
        .iter()
        .map(|call| call.id.clone())
        .collect();

    // Exactly one tool message, holding both results, bound by id.
    assert_eq!(history[assistant + 1].role, Role::Tool);
    let results = history[assistant + 1].tool_results();
    assert_eq!(results.len(), 2, "one message answers the whole batch");
    assert_eq!(results[0].tool_use_id, calls[0]);
    assert_eq!(results[0].name, "generate_image");
    assert_eq!(results[0].content, "rendered 1 image");
    assert_eq!(results[1].tool_use_id, calls[1]);
    assert_eq!(results[1].name, "echo");
    assert_eq!(
        history
            .iter()
            .filter(|message| message.role == Role::Tool)
            .count(),
        1,
        "no second tool message was pushed for the batch"
    );

    // The images payload comes after the batch, never between its results.
    let follow_up = &history[assistant + 2];
    assert_eq!(follow_up.role, Role::User);
    assert!(follow_up.text().contains("generate_image"));
    assert_eq!(follow_up.images().len(), 1);
}

#[tokio::test]
async fn tool_images_ride_back_on_a_following_user_message() {
    // The convention every provider tolerates: the tool message carries the
    // text, the images follow on a user message (a `tool` result cannot
    // carry image blocks on OpenAI).
    let image = test_png();
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(ImageTool {
        images: vec![image.clone()],
    }));

    let tmp = TempDir::new();
    let (mut agent, _provider) = test_agent_in(
        &tmp,
        vec![vec![calls_image_tool()], vec![final_chunk("done")]],
        Vec::new(),
        registry,
    );

    let (tx, mut rx) = mpsc::channel(64);
    agent.run_turn("make me a picture", tx).await.expect("turn");

    let history = agent.history();
    let tool_index = history
        .iter()
        .position(|message| message.role == Role::Tool)
        .expect("a tool result");
    assert_eq!(history[tool_index].text(), "rendered 1 image");
    assert!(
        history[tool_index].images().is_empty(),
        "the tool message carries the text only"
    );
    let follow_up = &history[tool_index + 1];
    assert_eq!(follow_up.role, Role::User);
    assert!(follow_up.text().contains("generate_image"));
    assert_eq!(follow_up.images().len(), 1, "the model sees it");
    assert_eq!(follow_up.images()[0].b64, image.b64);

    // The surfaces get the tool's images twice over: on ToolFinished (as
    // base64, for free) and on Images (as a path, which is what they use).
    let mut finished_images = Vec::new();
    let mut announced = Vec::new();
    while let Ok(event) = rx.try_recv() {
        match event {
            AgentEvent::ToolFinished { output, .. } => finished_images.extend(output.images),
            AgentEvent::Images { source, images } => announced.push((source, images)),
            _ => {}
        }
    }
    assert_eq!(finished_images.len(), 1);
    assert_eq!(finished_images[0].b64, image.b64);
    assert_eq!(announced.len(), 1);
    assert_eq!(announced[0].0, ImageSource::Tool("generate_image".into()));
    let saved = &announced[0].1[0];
    assert_eq!(
        std::fs::read(&saved.path).expect("the image file"),
        image.decode().unwrap()
    );
}

#[tokio::test]
async fn an_oversized_image_is_dropped_with_a_notice_and_never_enters_history() {
    // A runaway image must not melt the context window or the session file.
    let huge = Image::new(
        "A".repeat(crate::llm::MAX_IMAGE_BYTES / 3 * 4 + 8),
        "image/png",
    );
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(ImageTool {
        images: vec![huge, test_png()],
    }));

    let tmp = TempDir::new();
    let (mut agent, _provider) = test_agent_in(
        &tmp,
        vec![vec![calls_image_tool()], vec![final_chunk("done")]],
        Vec::new(),
        registry,
    );

    let (tx, mut rx) = mpsc::channel(64);
    agent.run_turn("make me a picture", tx).await.expect("turn");

    let follow_up = agent
        .history()
        .iter()
        .find(|message| message.role == Role::User && !message.images().is_empty())
        .expect("the images user message");
    assert_eq!(
        follow_up.images().len(),
        1,
        "the oversized image never reaches the model; the sane one does"
    );
    assert_eq!(follow_up.images()[0].b64, test_png().b64);

    let (_text, _errors, notices) = drain_events(&mut rx);
    assert!(
        notices.iter().any(|notice| notice.contains("oversized")),
        "the drop is surfaced, not silent: {notices:?}"
    );
}

/// `done: true` chunk carrying token counts alongside `content`.
fn usage_chunk(content: &str, prompt_tokens: u64, completion_tokens: u64) -> ChatChunk {
    ChatChunk {
        eval_count: Some(completion_tokens),
        prompt_eval_count: Some(prompt_tokens),
        cache: CacheTokens::NONE,
        ..final_chunk(content)
    }
}

/// [`usage_chunk`] whose prompt was partly served from the provider's cache.
///
/// `prompt_tokens` is the whole prompt, cached portion included — the shape
/// [`CacheTokens`] documents and the one every adapter reconciles to before
/// the chunk leaves it.
fn cached_usage_chunk(
    content: &str,
    prompt_tokens: u64,
    completion_tokens: u64,
    cache: CacheTokens,
) -> ChatChunk {
    ChatChunk {
        cache,
        ..usage_chunk(content, prompt_tokens, completion_tokens)
    }
}

fn test_agent(responses: Vec<Vec<ChatChunk>>) -> (Agent, Arc<ScriptedProvider>, TempDir) {
    let tmp = TempDir::new();
    let (agent, provider) = test_agent_in(&tmp, responses, Vec::new(), ToolRegistry::new());
    (agent, provider, tmp)
}

/// Build a test agent rooted in `tmp` with injected hook definitions and
/// a custom registry.
fn test_agent_in(
    tmp: &TempDir,
    responses: Vec<Vec<ChatChunk>>,
    hook_defs: Vec<HookDef>,
    registry: ToolRegistry,
) -> (Agent, Arc<ScriptedProvider>) {
    let provider = ScriptedProvider::new(responses);
    let agent = test_agent_with(tmp, Arc::clone(&provider), hook_defs, registry);
    (agent, provider)
}

/// Build a test agent around an existing provider. The usage log is
/// redirected into the temp dir so tests never touch ~/.wizard.
fn test_agent_with(
    tmp: &TempDir,
    provider: Arc<ScriptedProvider>,
    hook_defs: Vec<HookDef>,
    registry: ToolRegistry,
) -> Agent {
    let session = Session::create(&tmp.0).expect("create session");
    let hooks = Arc::new(HookEngine::new(
        hook_defs,
        tmp.0.clone(),
        session.id.clone(),
    ));
    let mut agent = Agent::new(
        provider,
        registry,
        Config::default(),
        Vec::new(),
        tmp.0.clone(),
        session,
        true,
        hooks,
    )
    .expect("build agent");
    agent.set_usage_log(Some(tmp.0.join("usage.jsonl")));
    agent
}

/// Drain a finished turn's events into (text, errors, notices).
fn drain_events(rx: &mut mpsc::Receiver<AgentEvent>) -> (String, Vec<String>, Vec<String>) {
    let mut text = String::new();
    let mut errors = Vec::new();
    let mut notices = Vec::new();
    while let Ok(event) = rx.try_recv() {
        match event {
            AgentEvent::TextDelta(delta) => text.push_str(&delta),
            AgentEvent::Error(message) => errors.push(message),
            AgentEvent::Notice(message) => notices.push(message),
            _ => {}
        }
    }
    (text, errors, notices)
}

/// Test tool that records the arguments of every call it receives.
struct RecordingTool {
    calls: Arc<Mutex<Vec<Value>>>,
}

#[async_trait::async_trait]
impl crate::tools::Tool for RecordingTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Echo the arguments back (test tool)."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolOutput, crate::tools::ToolError> {
        self.calls.lock().unwrap().push(args.clone());
        Ok(ToolOutput::ok(format!("echoed {args}")))
    }
}

/// Registry holding one [`RecordingTool`], plus the shared call log.
fn recording_registry() -> (ToolRegistry, Arc<Mutex<Vec<Value>>>) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(RecordingTool {
        calls: Arc::clone(&calls),
    }));
    (registry, calls)
}

/// `done: true` chunk carrying one tool call.
fn tool_call_chunk(name: &str, arguments: Value) -> ChatChunk {
    ChatChunk {
        message: Some(ChatMessage::assistant_turn(
            "",
            Vec::new(),
            vec![ToolCall::new(name.to_string(), arguments)],
        )),
        images: Vec::new(),
        thinking: false,
        done: true,
        done_reason: None,
        eval_count: None,
        prompt_eval_count: None,
        cache: CacheTokens::NONE,
    }
}

/// Write a hook script into `dir` and return the command that runs it
/// (via `sh`, so no exec bit is needed).
fn write_script(dir: &Path, name: &str, body: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, body).expect("write hook script");
    format!("sh {}", path.display())
}

fn hook(event: HookEvent, matcher: Option<&str>, command: String) -> HookDef {
    HookDef {
        event,
        matcher: matcher.map(str::to_string),
        command,
        timeout_secs: None,
    }
}

#[test]
fn completion_is_empty_requires_no_text_and_no_calls() {
    assert!(completion_is_empty("", &[]));
    assert!(completion_is_empty("  \n\t", &[]));
    assert!(!completion_is_empty("done", &[]));
    let call = ToolCall::new("execute".to_string(), json!({}));
    assert!(!completion_is_empty("", std::slice::from_ref(&call)));
    assert!(!completion_is_empty("done", &[call]));
}

#[tokio::test]
async fn empty_completion_retries_with_nudge_then_succeeds() {
    let (mut agent, provider, _tmp) = test_agent(vec![
        // First completion: reasoning-only stop, nothing visible.
        vec![final_chunk("")],
        // Retry after the nudge: a real reply.
        vec![final_chunk("Here are my findings.")],
    ]);

    let (tx, mut rx) = mpsc::channel(64);
    let reason = agent.run_turn("hi", tx).await.expect("turn ok");
    assert_eq!(reason, DoneReason::Completed);

    let (text, errors, _notices) = drain_events(&mut rx);
    assert_eq!(text, "Here are my findings.");
    assert!(errors.is_empty(), "no notice on a successful retry");

    // The retry request carried the nudge as its final user message.
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let nudge = requests[1].messages.last().expect("retry has messages");
    assert_eq!(nudge.role, Role::User);
    assert_eq!(nudge.text(), EMPTY_COMPLETION_NUDGE);

    // The nudge never lands in history or the persisted session.
    assert!(
        agent
            .history()
            .iter()
            .all(|m| m.text() != EMPTY_COMPLETION_NUDGE),
        "nudge is not kept in history"
    );
    let persisted = agent.session().load_messages().expect("session readable");
    assert!(
        persisted.iter().all(|m| m.text() != EMPTY_COMPLETION_NUDGE),
        "nudge is not persisted"
    );
    assert!(
        persisted
            .iter()
            .any(|m| m.text() == "Here are my findings."),
        "the real reply is persisted"
    );
}

#[tokio::test]
async fn double_empty_completion_surfaces_a_notice() {
    let (mut agent, provider, _tmp) =
        test_agent(vec![vec![final_chunk("")], vec![final_chunk("")]]);

    let (tx, mut rx) = mpsc::channel(64);
    let reason = agent.run_turn("hi", tx).await.expect("turn ok");
    assert_eq!(reason, DoneReason::Completed);

    let (text, errors, _notices) = drain_events(&mut rx);
    assert!(text.is_empty());
    assert!(
        errors.iter().any(|e| e.contains("empty response")),
        "visible notice emitted: {errors:?}"
    );

    assert_eq!(provider.requests.lock().unwrap().len(), 2, "retried once");
    // No empty assistant message is recorded.
    assert!(
        agent
            .history()
            .iter()
            .all(|m| m.role != Role::Assistant || !m.text().is_empty()),
        "no empty assistant message in history"
    );
}

#[test]
fn parses_whole_message_protocol_call() {
    let call = parse_json_tool_call(r#"{"tool":"read_file","arguments":{"path":"src/lib.rs"}}"#)
        .expect("valid protocol call");
    assert_eq!(call.function.name, "read_file");
    assert_eq!(call.function.arguments["path"], "src/lib.rs");
}

#[test]
fn parses_fenced_json_block_with_prose() {
    let text = "I'll check the diff first.\n```json\n{\"tool\":\"git_diff\",\"arguments\":{\"staged\":true}}\n```\nThen I'll proceed.";
    let call = parse_json_tool_call(text).expect("fenced call parses");
    assert_eq!(call.function.name, "git_diff");
    assert_eq!(call.function.arguments["staged"], true);
}

#[test]
fn parses_fence_without_language_tag() {
    let text = "```\n{\"tool\":\"git_status\"}\n```";
    let call = parse_json_tool_call(text).expect("bare fence parses");
    assert_eq!(call.function.name, "git_status");
}

#[test]
fn parses_single_json_line_inside_prose() {
    let text = "Let me list the files.\n{\"tool\":\"list_files\",\"arguments\":{\"path\":\".\"}}\nThat should do it.";
    let call = parse_json_tool_call(text).expect("inline line parses");
    assert_eq!(call.function.name, "list_files");
}

#[test]
fn missing_arguments_default_to_empty_object() {
    let call = parse_json_tool_call(r#"{"tool":"git_status"}"#).expect("parses");
    assert_eq!(call.function.arguments, json!({}));

    let call = parse_json_tool_call(r#"{"tool":"git_status","arguments":null}"#).expect("parses");
    assert_eq!(call.function.arguments, json!({}));
}

#[test]
fn plain_text_and_non_tool_json_are_not_calls() {
    assert!(parse_json_tool_call("I finished the task. All tests pass.").is_none());
    assert!(parse_json_tool_call(r#"{"result": "done"}"#).is_none());
    assert!(parse_json_tool_call("```json\n{\"answer\": 42}\n```").is_none());
    assert!(parse_json_tool_call("").is_none());
}

#[test]
fn normalize_args_handles_null_and_double_encoding() {
    assert_eq!(normalize_args(&Value::Null), json!({}));
    // Some models double-encode arguments as a JSON string.
    assert_eq!(
        normalize_args(&json!("{\"path\":\"a.rs\"}")),
        json!({ "path": "a.rs" })
    );
    // A plain (non-JSON) string is passed through untouched.
    assert_eq!(normalize_args(&json!("not json")), json!("not json"));
    // Objects pass through.
    assert_eq!(normalize_args(&json!({ "k": 1 })), json!({ "k": 1 }));
}

#[test]
fn loop_control_parses_known_commands() {
    let tmp = TempDir::new();
    let control_dir = tmp.0.join(".wizard");
    std::fs::create_dir_all(&control_dir).unwrap();

    for (content, expected) in [
        ("stop", LoopControl::Stop),
        ("  PAUSE \n", LoopControl::Pause),
        ("Skip", LoopControl::Skip),
    ] {
        std::fs::write(control_dir.join("loop-control"), content).unwrap();
        assert_eq!(
            read_loop_control(&tmp.0),
            Some(expected),
            "content {content:?}"
        );
    }

    std::fs::write(control_dir.join("loop-control"), "resume").unwrap();
    assert_eq!(read_loop_control(&tmp.0), None, "resume means no command");
    std::fs::write(control_dir.join("loop-control"), "gibberish").unwrap();
    assert_eq!(read_loop_control(&tmp.0), None);
}

/// Write `pause` into the project's `.wizard/loop-control`.
fn pause_the_loop(root: &Path) {
    let dir = root.join(".wizard");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("loop-control"), "pause").unwrap();
}

/// A paused run still answers Ctrl-C and still stops at `--max-hours`.
///
/// `pause` parked the loop in a two-second sleep and re-read the file,
/// observing nothing else. So a `pause` left behind — by an operator who went
/// home, or by a script that crashed before writing the release — outlived
/// Ctrl-C and ran straight through the deadline, indefinitely: the two checks
/// that would have ended the run are at the *top* of the step, which a paused
/// run never reaches.
#[tokio::test]
async fn a_paused_run_still_honors_cancellation_and_the_deadline() {
    let tmp = TempDir::new();
    let (mut agent, _provider) = test_agent_in(&tmp, Vec::new(), Vec::new(), ToolRegistry::new());
    pause_the_loop(&tmp.0);

    // Ctrl-C while paused ends the run.
    agent.cancel_handle().cancel();
    let policy = turn::Policy::turn(&agent);
    let mut host = turn::TurnHost { agent: &mut agent };
    assert!(
        matches!(
            turn::honor_loop_control(&mut host, &policy).await,
            Some(DoneReason::Stopped)
        ),
        "a paused run must still stop on cancellation"
    );

    // A deadline that falls *during* the pause ends it too, without waiting
    // out the poll interval — the sleep is raced against the deadline, not
    // merely re-checked after it.
    agent.cancel_handle().clear();
    agent.set_deadline(Some(Instant::now() + Duration::from_millis(50)));
    let policy = turn::Policy::turn(&agent);
    let mut host = turn::TurnHost { agent: &mut agent };
    let started = Instant::now();
    assert!(
        matches!(
            turn::honor_loop_control(&mut host, &policy).await,
            Some(DoneReason::TimeLimit)
        ),
        "a paused run must still stop at its deadline"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the deadline was waited out rather than raced: {:?}",
        started.elapsed()
    );

    // And the file is still what releases an otherwise-unbounded pause.
    agent.set_deadline(None);
    std::fs::write(tmp.0.join(".wizard").join("loop-control"), "resume").unwrap();
    let policy = turn::Policy::turn(&agent);
    let mut host = turn::TurnHost { agent: &mut agent };
    assert!(
        turn::honor_loop_control(&mut host, &policy).await.is_none(),
        "resume lets the run continue"
    );
}

#[test]
fn loop_control_absent_file_is_none() {
    let tmp = TempDir::new();
    assert_eq!(read_loop_control(&tmp.0), None);
}

#[tokio::test]
async fn pre_tool_use_block_feeds_reason_to_model_as_tool_error() {
    let tmp = TempDir::new();
    let command = write_script(
        &tmp.0,
        "block.sh",
        "echo 'no echoing allowed' >&2\nexit 2\n",
    );
    let (registry, calls) = recording_registry();
    let (mut agent, provider) = test_agent_in(
        &tmp,
        vec![
            vec![tool_call_chunk("echo", json!({ "text": "hi" }))],
            vec![final_chunk("understood")],
        ],
        vec![hook(HookEvent::PreToolUse, Some("echo"), command)],
        registry,
    );

    let (tx, _rx) = mpsc::channel(64);
    let reason = agent.run_turn("go", tx).await.expect("turn ok");
    assert_eq!(reason, DoneReason::Completed);
    assert!(calls.lock().unwrap().is_empty(), "blocked tool never ran");

    // The block reason reached the model as an ordinary tool error.
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let feedback = requests[1]
        .messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Tool)
        .expect("tool feedback message");
    assert!(
        feedback.text().contains("blocked by pre_tool_use hook"),
        "{}",
        feedback.text()
    );
    assert!(feedback.text().contains("no echoing allowed"));
}

#[tokio::test]
async fn pre_tool_use_updated_args_rewrite_the_call() {
    let tmp = TempDir::new();
    let command = write_script(
        &tmp.0,
        "rewrite.sh",
        "echo '{\"updated_args\": {\"text\": \"rewritten\"}}'\n",
    );
    let (registry, calls) = recording_registry();
    let (mut agent, _provider) = test_agent_in(
        &tmp,
        vec![
            vec![tool_call_chunk("echo", json!({ "text": "original" }))],
            vec![final_chunk("done")],
        ],
        vec![hook(HookEvent::PreToolUse, None, command)],
        registry,
    );

    let (tx, _rx) = mpsc::channel(64);
    let reason = agent.run_turn("go", tx).await.expect("turn ok");
    assert_eq!(reason, DoneReason::Completed);
    assert_eq!(
        *calls.lock().unwrap(),
        vec![json!({ "text": "rewritten" })],
        "the tool ran with the hook's arguments"
    );
}

#[tokio::test]
async fn post_tool_use_stdout_is_appended_to_the_result() {
    let tmp = TempDir::new();
    let command = write_script(&tmp.0, "annotate.sh", "echo 'lint: all clean'\n");
    let (registry, calls) = recording_registry();
    let (mut agent, provider) = test_agent_in(
        &tmp,
        vec![
            vec![tool_call_chunk("echo", json!({ "text": "hi" }))],
            vec![final_chunk("done")],
        ],
        vec![hook(HookEvent::PostToolUse, Some("echo"), command)],
        registry,
    );

    let (tx, _rx) = mpsc::channel(64);
    agent.run_turn("go", tx).await.expect("turn ok");
    assert_eq!(calls.lock().unwrap().len(), 1, "the tool ran normally");

    let requests = provider.requests.lock().unwrap();
    let feedback = requests[1]
        .messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Tool)
        .expect("tool feedback message");
    assert!(feedback.text().contains("echoed"), "{}", feedback.text());
    assert!(
        feedback.text().contains("lint: all clean"),
        "hook stdout appended: {}",
        feedback.text()
    );
}

#[tokio::test]
async fn user_prompt_submit_block_ends_the_turn() {
    let tmp = TempDir::new();
    let command = write_script(
        &tmp.0,
        "veto.sh",
        "echo 'not during business hours' >&2\nexit 2\n",
    );
    let (mut agent, provider) = test_agent_in(
        &tmp,
        Vec::new(), // the model must never be asked
        vec![hook(HookEvent::UserPromptSubmit, None, command)],
        ToolRegistry::new(),
    );

    let (tx, mut rx) = mpsc::channel(64);
    let reason = agent.run_turn("do the thing", tx).await.expect("turn ok");
    assert_eq!(reason, DoneReason::Stopped);
    assert!(provider.requests.lock().unwrap().is_empty());
    assert_eq!(agent.history().len(), 1, "the prompt never entered history");

    let (_text, errors, _notices) = drain_events(&mut rx);
    assert!(
        errors
            .iter()
            .any(|e| e.contains("blocked") && e.contains("not during business hours")),
        "notice emitted: {errors:?}"
    );
}

#[tokio::test]
async fn user_prompt_submit_stdout_is_appended_to_the_message() {
    let tmp = TempDir::new();
    let command = write_script(&tmp.0, "context.sh", "echo 'remember: deploy is frozen'\n");
    let (mut agent, provider) = test_agent_in(
        &tmp,
        vec![vec![final_chunk("noted")]],
        vec![hook(HookEvent::UserPromptSubmit, None, command)],
        ToolRegistry::new(),
    );

    let (tx, _rx) = mpsc::channel(64);
    agent.run_turn("do the thing", tx).await.expect("turn ok");

    let requests = provider.requests.lock().unwrap();
    let prompt = requests[0].messages.last().expect("user message");
    assert_eq!(prompt.role, Role::User);
    assert!(prompt.text().contains("do the thing"), "{}", prompt.text());
    assert!(
        prompt.text().contains("remember: deploy is frozen"),
        "hook context appended: {}",
        prompt.text()
    );
}

/// Run a turn while a reviewer task answers every [`AgentEvent::PlanReady`]
/// with `verdict`. Returns (done reason, plans that were presented).
async fn run_turn_with_reviewer(
    agent: &mut Agent,
    input: &str,
    verdict: PlanVerdict,
) -> (DoneReason, Vec<String>) {
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
    let reviewer = async move {
        let mut plans = Vec::new();
        while let Some(event) = rx.recv().await {
            if let AgentEvent::PlanReady { plan, gate } = event {
                plans.push(plan);
                assert!(gate.answer(verdict.clone()), "verdict delivered");
            }
        }
        plans
    };
    let (reason, plans) = tokio::join!(agent.run_turn(input, tx), reviewer);
    (reason.expect("turn ok"), plans)
}

/// Last tool-result message of request `index`, as fed to the model.
fn tool_feedback_of(provider: &ScriptedProvider, index: usize) -> String {
    let requests = provider.requests.lock().unwrap();
    requests[index]
        .messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Tool)
        .expect("tool feedback message")
        .text()
        .clone()
}

#[tokio::test]
async fn plan_mode_blocks_non_read_only_tools_but_the_turn_continues() {
    let tmp = TempDir::new();
    let (mut agent, provider) = test_agent_in(
        &tmp,
        vec![
            vec![tool_call_chunk(
                "write_file",
                json!({ "path": "a.txt", "content": "x" }),
            )],
            vec![final_chunk("understood, planning instead")],
        ],
        Vec::new(),
        ToolRegistry::with_native_tools(),
    );
    agent.set_plan_mode(true);

    let (tx, _rx) = mpsc::channel(64);
    let reason = agent.run_turn("go", tx).await.expect("turn ok");
    assert_eq!(reason, DoneReason::Completed);
    assert!(!tmp.0.join("a.txt").exists(), "the write never happened");

    let feedback = tool_feedback_of(&provider, 1);
    assert!(
        feedback.contains("blocked by plan mode"),
        "block reason fed to the model: {feedback}"
    );
    assert!(feedback.contains("exit_plan"), "{feedback}");
    assert!(agent.plan_mode(), "plan mode stays on");

    // The system prompt carried the plan-mode instructions.
    let requests = provider.requests.lock().unwrap();
    assert!(requests[0].messages[0].text().contains("PLAN MODE"));
}

#[tokio::test]
async fn plan_mode_allows_read_only_tools() {
    let tmp = TempDir::new();
    std::fs::write(tmp.0.join("notes.txt"), "remember the milk\n").unwrap();
    let (mut agent, provider) = test_agent_in(
        &tmp,
        vec![
            vec![tool_call_chunk("read_file", json!({ "path": "notes.txt" }))],
            vec![final_chunk("read it")],
        ],
        Vec::new(),
        ToolRegistry::with_native_tools(),
    );
    agent.set_plan_mode(true);

    let (tx, _rx) = mpsc::channel(64);
    let reason = agent.run_turn("go", tx).await.expect("turn ok");
    assert_eq!(reason, DoneReason::Completed);
    let feedback = tool_feedback_of(&provider, 1);
    assert!(
        feedback.contains("remember the milk"),
        "read-only tools run normally: {feedback}"
    );
}

#[tokio::test]
async fn plan_mode_blocks_are_exempt_from_the_identical_failure_breaker() {
    // Sovereign's breaker trips after 3 identical failures; a planning
    // model probing the same write repeatedly must not end the turn.
    let tmp = TempDir::new();
    let write = || {
        vec![tool_call_chunk(
            "write_file",
            json!({ "path": "a", "content": "x" }),
        )]
    };
    let (mut agent, _provider) = test_agent_in(
        &tmp,
        vec![
            write(),
            write(),
            write(),
            write(),
            vec![final_chunk("fine, I will plan")],
        ],
        Vec::new(),
        ToolRegistry::with_native_tools(),
    );
    agent.set_mode(Mode::Sovereign);
    agent.set_plan_mode(true);

    let (tx, _rx) = mpsc::channel(256);
    let reason = agent.run_turn("go", tx).await.expect("turn ok");
    assert_eq!(reason, DoneReason::Completed, "no circuit breaker");
}

#[tokio::test]
async fn exit_plan_approval_writes_the_plan_and_clears_plan_mode() {
    let tmp = TempDir::new();
    let (mut agent, provider) = test_agent_in(
        &tmp,
        vec![
            vec![tool_call_chunk(
                "exit_plan",
                json!({ "plan": "# Plan\n1. do x" }),
            )],
            vec![final_chunk("executing the plan")],
        ],
        Vec::new(),
        ToolRegistry::new(),
    );
    agent.set_plan_mode(true);

    let (reason, plans) = run_turn_with_reviewer(&mut agent, "go", PlanVerdict::approve()).await;
    assert_eq!(reason, DoneReason::Completed);
    assert_eq!(plans, ["# Plan\n1. do x"]);
    assert!(!agent.plan_mode(), "approval clears plan mode");

    let saved =
        std::fs::read_to_string(tmp.0.join(".wizard").join("plan.md")).expect("plan persisted");
    assert_eq!(saved, "# Plan\n1. do x");

    let feedback = tool_feedback_of(&provider, 1);
    assert!(
        feedback.contains("Plan approved"),
        "the model is told to execute: {feedback}"
    );
    // After approval, the system prompt no longer carries the plan block.
    let requests = provider.requests.lock().unwrap();
    assert!(requests[0].messages[0].text().contains("PLAN MODE"));
    assert!(!requests[1].messages[0].text().contains("PLAN MODE"));
}

#[tokio::test]
async fn exit_plan_rejection_keeps_plan_mode_and_feeds_back_the_feedback() {
    let tmp = TempDir::new();
    let (mut agent, provider) = test_agent_in(
        &tmp,
        vec![
            vec![tool_call_chunk("exit_plan", json!({ "plan": "# v1" }))],
            vec![final_chunk("revising the plan")],
        ],
        Vec::new(),
        ToolRegistry::new(),
    );
    agent.set_plan_mode(true);

    let (reason, plans) =
        run_turn_with_reviewer(&mut agent, "go", PlanVerdict::reject("add tests first")).await;
    assert_eq!(reason, DoneReason::Completed);
    assert_eq!(plans.len(), 1);
    assert!(agent.plan_mode(), "rejection keeps plan mode on");

    let feedback = tool_feedback_of(&provider, 1);
    assert!(feedback.starts_with("Error:"), "{feedback}");
    assert!(feedback.contains("add tests first"), "{feedback}");
    assert!(
        feedback.contains("call exit_plan again"),
        "the model is told to retry: {feedback}"
    );
}

#[tokio::test]
async fn exit_plan_outside_plan_mode_is_an_error() {
    let tmp = TempDir::new();
    let (mut agent, provider) = test_agent_in(
        &tmp,
        vec![
            vec![tool_call_chunk("exit_plan", json!({ "plan": "# p" }))],
            vec![final_chunk("ok")],
        ],
        Vec::new(),
        ToolRegistry::new(),
    );

    let (tx, _rx) = mpsc::channel(64);
    let reason = agent.run_turn("go", tx).await.expect("turn ok");
    assert_eq!(reason, DoneReason::Completed);
    let feedback = tool_feedback_of(&provider, 1);
    assert!(feedback.contains("not in plan mode"), "{feedback}");
    assert!(
        !tmp.0.join(".wizard").join("plan.md").exists(),
        "no plan file written"
    );
}

#[tokio::test]
async fn headless_two_phase_turn_blocks_then_plans_then_executes() {
    // The --plan shape: write blocked while planning → exit_plan
    // auto-approved → the same write succeeds in the same turn.
    let tmp = TempDir::new();
    let write_args = json!({ "path": "result.txt", "content": "done" });
    let (mut agent, provider) = test_agent_in(
        &tmp,
        vec![
            vec![tool_call_chunk("write_file", write_args.clone())],
            vec![tool_call_chunk(
                "exit_plan",
                json!({ "plan": "# write result.txt" }),
            )],
            vec![tool_call_chunk("write_file", write_args)],
            vec![final_chunk("all done")],
        ],
        Vec::new(),
        ToolRegistry::with_native_tools(),
    );
    agent.set_plan_mode(true);

    let (reason, plans) = run_turn_with_reviewer(&mut agent, "go", PlanVerdict::approve()).await;
    assert_eq!(reason, DoneReason::Completed);
    assert_eq!(plans, ["# write result.txt"]);
    assert!(!agent.plan_mode());

    // The phases happened in order: blocked, approved, executed.
    assert!(
        tool_feedback_of(&provider, 1).contains("blocked by plan mode"),
        "phase 1: the write is blocked"
    );
    assert!(
        tool_feedback_of(&provider, 2).contains("Plan approved"),
        "phase 2: the plan is approved"
    );
    let executed = tool_feedback_of(&provider, 3);
    assert!(
        !executed.contains("blocked") && !executed.starts_with("Error:"),
        "phase 3: the write succeeds: {executed}"
    );
    let written = std::fs::read_to_string(tmp.0.join("result.txt")).expect("file written");
    assert_eq!(written, "done");
}

#[test]
fn exit_plan_is_always_registered() {
    let tmp = TempDir::new();
    let (mut agent, _provider) = test_agent_in(&tmp, Vec::new(), Vec::new(), ToolRegistry::new());
    let has_exit_plan = |agent: &Agent| {
        agent
            .dispatcher
            .registry()
            .get(crate::tools::plan::EXIT_PLAN_TOOL_NAME)
            .is_some()
    };
    assert!(has_exit_plan(&agent), "registered at construction");
    // A registry swap (/reload, /evolve) re-registers it.
    agent.set_registry(ToolRegistry::new());
    assert!(has_exit_plan(&agent), "re-registered after set_registry");
}

#[tokio::test]
async fn rewind_restores_an_overwritten_file_and_truncates_history() {
    let tmp = TempDir::new();
    let file = tmp.0.join("notes.txt");
    std::fs::write(&file, "before").unwrap();
    let (mut agent, _provider) = test_agent_in(
        &tmp,
        vec![
            vec![tool_call_chunk(
                "write_file",
                json!({ "path": "notes.txt", "content": "after" }),
            )],
            vec![final_chunk("overwritten")],
        ],
        Vec::new(),
        ToolRegistry::with_native_tools(),
    );

    let (tx, mut rx) = mpsc::channel(256);
    let reason = agent.run_turn("overwrite notes.txt", tx).await.unwrap();
    drain_events(&mut rx);
    assert_eq!(reason, DoneReason::Completed);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "after");
    assert!(agent.history().len() > 1);

    let candidates = agent.rewind_candidates(10);
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].prompt, "overwrite notes.txt");
    assert_eq!(candidates[0].files, vec![file.clone()]);

    let restored = agent.rewind_to(candidates[0].turn).unwrap();
    assert_eq!(restored, vec![file.clone()]);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "before");
    assert_eq!(
        agent.history().len(),
        1,
        "only the system prompt survives a full rewind"
    );
    assert!(
        agent.session().load_messages().unwrap().is_empty(),
        "the session file was truncated"
    );
}

#[tokio::test]
async fn rewind_deletes_a_file_the_turn_created() {
    let tmp = TempDir::new();
    let file = tmp.0.join("created.txt");
    let (mut agent, _provider) = test_agent_in(
        &tmp,
        vec![
            vec![tool_call_chunk(
                "write_file",
                json!({ "path": "created.txt", "content": "fresh" }),
            )],
            vec![final_chunk("created")],
        ],
        Vec::new(),
        ToolRegistry::with_native_tools(),
    );

    let (tx, mut rx) = mpsc::channel(256);
    agent.run_turn("create created.txt", tx).await.unwrap();
    drain_events(&mut rx);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "fresh");

    let candidates = agent.rewind_candidates(10);
    assert_eq!(candidates.len(), 1);
    agent.rewind_to(candidates[0].turn).unwrap();
    assert!(!file.exists(), "rewind deletes a file that did not exist");
}

#[tokio::test]
async fn rewind_to_a_later_turn_keeps_earlier_turns() {
    let tmp = TempDir::new();
    let file = tmp.0.join("notes.txt");
    std::fs::write(&file, "v0").unwrap();
    let (mut agent, _provider) = test_agent_in(
        &tmp,
        vec![
            vec![tool_call_chunk(
                "write_file",
                json!({ "path": "notes.txt", "content": "v1" }),
            )],
            vec![final_chunk("first done")],
            vec![tool_call_chunk(
                "write_file",
                json!({ "path": "notes.txt", "content": "v2" }),
            )],
            vec![final_chunk("second done")],
        ],
        Vec::new(),
        ToolRegistry::with_native_tools(),
    );

    let (tx, mut rx) = mpsc::channel(256);
    agent.run_turn("write v1", tx.clone()).await.unwrap();
    agent.run_turn("write v2", tx).await.unwrap();
    drain_events(&mut rx);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "v2");

    let candidates = agent.rewind_candidates(10);
    assert_eq!(candidates.len(), 2, "newest first");
    assert!(candidates[0].turn > candidates[1].turn);

    agent.rewind_to(candidates[0].turn).unwrap();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "v1");
    let messages = agent.session().load_messages().unwrap();
    assert_eq!(
        messages.first().map(|m| m.text()).as_deref(),
        Some("write v1"),
        "the first turn's history survives"
    );
    assert!(
        messages.iter().all(|m| m.text() != "write v2"),
        "the second turn's history is gone"
    );
}

#[tokio::test]
async fn rollback_failed_cycle_restores_files_and_notes_the_mission() {
    let tmp = TempDir::new();
    let file = tmp.0.join("data.txt");
    std::fs::write(&file, "good").unwrap();
    let (mut agent, _provider) = test_agent_in(
        &tmp,
        vec![
            vec![tool_call_chunk(
                "write_file",
                json!({ "path": "data.txt", "content": "broken" }),
            )],
            vec![final_chunk("changed it")],
        ],
        Vec::new(),
        ToolRegistry::with_native_tools(),
    );

    let cycle_first_turn = agent.checkpoints().current_turn() + 1;
    let (tx, mut rx) = mpsc::channel(256);
    agent.run_turn("break the data", tx).await.unwrap();
    drain_events(&mut rx);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "broken");

    let spinner = crate::progress::TurnSpinner::new();
    let mut mission = mission::Mission::new("keep the data good");

    // Disabled: a no-op.
    let config = Config::default();
    rollback_failed_cycle(
        &config,
        &agent,
        Some(&mut mission),
        &tmp.0,
        cycle_first_turn,
        "circuit breaker",
        Some(&spinner),
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "broken");
    assert!(mission.notes.is_empty());

    // Enabled: the cycle's edits are restored and the mission notes it.
    let config = Config {
        rollback_failed_cycles: true,
        ..Config::default()
    };
    rollback_failed_cycle(
        &config,
        &agent,
        Some(&mut mission),
        &tmp.0,
        cycle_first_turn,
        "circuit breaker",
        Some(&spinner),
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "good");
    assert!(
        mission.notes.last().is_some_and(
            |note| note.contains("rolled back 1 file(s)") && note.contains("circuit breaker")
        ),
        "rollback noted in the mission: {:?}",
        mission.notes
    );
    // The note was persisted to mission.toml.
    let loaded = mission::Mission::load(&tmp.0).unwrap().expect("saved");
    assert_eq!(loaded.notes, mission.notes);
}

#[tokio::test]
async fn usage_counts_accumulate_emit_events_and_land_in_the_jsonl_log() {
    let tmp = TempDir::new();
    let provider = ScriptedProvider::new(vec![
        vec![usage_chunk("first", 100, 20)],
        vec![usage_chunk("second", 150, 30)],
    ]);
    let mut agent = test_agent_with(&tmp, Arc::clone(&provider), Vec::new(), ToolRegistry::new());

    let (tx, mut rx) = mpsc::channel(64);
    agent.run_turn("one", tx).await.expect("turn ok");
    let mut usage_events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let AgentEvent::Usage {
            prompt_tokens,
            completion_tokens,
        } = event
        {
            usage_events.push((prompt_tokens, completion_tokens));
        }
    }
    assert_eq!(usage_events, [(100, 20)], "one Usage event per model call");

    let (tx, _rx) = mpsc::channel(64);
    agent.run_turn("two", tx).await.expect("turn ok");

    assert_eq!(agent.usage().session_totals(), (250, 50));
    assert_eq!(agent.usage().turn_totals(), (150, 30), "last turn only");
    assert_eq!(agent.usage().last_prompt_tokens(), Some(150));

    // One JSONL record per turn, in order.
    let raw = std::fs::read_to_string(tmp.0.join("usage.jsonl")).expect("log written");
    let records: Vec<crate::usage::UsageRecord> = raw
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid json"))
        .collect();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].prompt_tokens, 100);
    assert_eq!(records[0].completion_tokens, 20);
    assert_eq!(records[1].prompt_tokens, 150);
    assert_eq!(records[1].completion_tokens, 30);
    assert_eq!(records[0].mode, "genie");
    assert!(records[0].ts > 0);
    assert_eq!(records[0].project, tmp.0.display().to_string());

    // Cost is settled at write time, not left for the reader: every record
    // carries a figure and the provenance of the rate that produced it. The
    // default config is the synthesized llama.cpp provider, so these turns are
    // the self-hosted case: $0.00, and labelled as such rather than priced at
    // the unknown-model fallback. That pins both halves of the wiring: drop
    // `cost_usd` and the first assertion fires, stop passing the provider's
    // kind through and the source becomes `Fallback` with a non-zero cost.
    assert_eq!(records[0].cost_usd, Some(0.0));
    assert_eq!(records[0].price_source, crate::usage::PriceSource::Local);
    assert_eq!(records[1].price_source, crate::usage::PriceSource::Local);
    // These chunks carry `CacheTokens::NONE`, which is what a backend with no
    // prompt cache reports, so the subset counts are zero rather than absent.
    // The turn that *does* report a split is
    // `cached_prompt_tokens_reach_the_usage_record_and_the_price`.
    assert_eq!(records[0].cache_read_tokens, 0);
    assert_eq!(records[0].cache_write_tokens, 0);
}

/// Exit criterion 8's second clause: `wizard usage` shows real cost including
/// cached-token pricing.
///
/// The seam this covers was built correctly on both ends and never joined in
/// the middle: `record_cache` was defined, `ModelPrice` priced a cache read at
/// 0.1x input and a write at 1.25x, `UsageRecord` carried both counts and
/// `wizard usage` had a `cached` column — and no production code ever reported
/// a hit, because `ChatChunk` had nowhere to put the number the adapters were
/// already decoding. Every turn therefore billed as all-fresh input.
///
/// This drives the whole path in one go: a provider reports a split on its
/// final chunk, the turn loop hands it to the tracker, and the record on disk
/// carries it and is priced by it. Break any link — drop `ChatChunk::cache`,
/// stop calling `record_cache`, stop passing the counts to `estimate_cost` —
/// and one of the three blocks below fails.
#[tokio::test]
async fn cached_prompt_tokens_reach_the_usage_record_and_the_price() {
    let tmp = TempDir::new();
    // A metered model with a table price, so the cost column is a real
    // number rather than the self-hosted $0.00 every other test here sees.
    let mut config = Config::default();
    config.providers = vec![crate::config::ProviderConfig {
        name: "anthropic".to_string(),
        kind: crate::config::ProviderKind::Anthropic,
        base_url: "https://api.anthropic.com".to_string(),
        model: "claude-opus-5".to_string(),
        api_key_env: None,
        gguf_path: None,
        usd_per_mtok_in: None,
        usd_per_mtok_out: None,
    }];
    config.active_provider = Some("anthropic".to_string());

    // 100,000 prompt tokens, 90,000 of them read back from the cache and
    // 5,000 written into it. That is one `/ultra` candidate's worth of a
    // re-sent charter, which is the case caching exists for.
    let provider = ScriptedProvider::new(vec![vec![cached_usage_chunk(
        "warm",
        100_000,
        1_000,
        CacheTokens {
            read: 90_000,
            write: 5_000,
        },
    )]]);
    let session = Session::create(&tmp.0).expect("create session");
    let hooks = Arc::new(HookEngine::new(
        Vec::new(),
        tmp.0.clone(),
        session.id.clone(),
    ));
    let mut agent = Agent::new(
        provider,
        ToolRegistry::new(),
        config,
        Vec::new(),
        tmp.0.clone(),
        session,
        true,
        hooks,
    )
    .expect("build agent");
    agent.set_usage_log(Some(tmp.0.join("usage.jsonl")));

    let (tx, _rx) = mpsc::channel(64);
    agent.run_turn("go", tx).await.expect("turn ok");

    // 1. The counters saw it.
    assert_eq!(agent.usage().turn_cache_totals(), (90_000, 5_000));
    assert_eq!(agent.usage().session_cache_totals(), (90_000, 5_000));
    assert_eq!(
        agent.usage().turn_totals(),
        (100_000, 1_000),
        "the cache split is a subset of the prompt, never an addition to it"
    );

    // 2. The record on disk carries it.
    let raw = std::fs::read_to_string(tmp.0.join("usage.jsonl")).expect("log written");
    let record: crate::usage::UsageRecord =
        serde_json::from_str(raw.lines().next().expect("one record")).expect("valid json");
    assert_eq!(record.prompt_tokens, 100_000);
    assert_eq!(record.cache_read_tokens, 90_000);
    assert_eq!(record.cache_write_tokens, 5_000);
    assert_eq!(record.price_source, crate::usage::PriceSource::Table);

    // 3. The cost is the cached one, not the all-fresh one. claude-opus-5 is
    //    $5/Mtok in and $25/Mtok out, so:
    //      5,000 fresh      x $5.00   = $0.025
    //     90,000 cache read x $0.50   = $0.045
    //      5,000 cache write x $6.25  = $0.03125
    //      1,000 output     x $25.00  = $0.025
    //                                 = $0.12625
    //    Billed as all-fresh input the same turn is 100,000 x $5.00 + output
    //    = $0.525, which is 4.2x too much — and up to 10x on the cached
    //    portion alone.
    let billed = record.cost_usd.expect("a priced record");
    assert!((billed - 0.126_25).abs() < 1e-9, "{record:?}");
    let all_fresh = crate::usage::estimate_cost(
        crate::usage::TurnTokens {
            prompt: 100_000,
            completion: 1_000,
            cache_read: 0,
            cache_write: 0,
        },
        &crate::usage::PriceInputs {
            model: "claude-opus-5",
            endpoint: "https://api.anthropic.com",
            usd_per_mtok_in: None,
            usd_per_mtok_out: None,
            self_hosted: false,
        },
    );
    assert!(
        billed < all_fresh.usd * 0.5,
        "a turn that was mostly a cache hit must not be billed like a cold \
         one: billed {billed}, all-fresh {}",
        all_fresh.usd
    );
}

#[tokio::test]
async fn turns_without_reported_counts_write_no_usage_records() {
    let (mut agent, _provider, tmp) = test_agent(vec![vec![final_chunk("plain")]]);
    let (tx, _rx) = mpsc::channel(64);
    agent.run_turn("go", tx).await.expect("turn ok");
    assert_eq!(agent.usage().session_totals(), (0, 0));
    assert!(!tmp.0.join("usage.jsonl").exists(), "no counts, no log");
}

#[tokio::test]
async fn prompt_tokens_near_the_context_window_trigger_compaction() {
    let tmp = TempDir::new();
    // Window 1000 → compaction at >800 prompt tokens. The byte threshold
    // (48k) is never reached: the messages are tiny.
    let provider = ScriptedProvider::with_context_window(
        vec![
            // Turn 1: reports a prompt size of 900 tokens.
            vec![usage_chunk("ok", 900, 10)],
            // Turn 2: the compaction summary, then the actual reply.
            vec![final_chunk("progress so far")],
            vec![final_chunk("done")],
        ],
        1000,
    );
    let mut agent = test_agent_with(&tmp, Arc::clone(&provider), Vec::new(), ToolRegistry::new());
    for i in 0..14 {
        agent.history.push(ChatMessage::user(format!("filler {i}")));
    }

    let (tx, _rx) = mpsc::channel(64);
    agent.run_turn("one", tx).await.expect("turn ok");
    assert!(
        !agent
            .history
            .iter()
            .any(|m| m.text().contains("[Compacted progress summary]")),
        "no compaction before a token count arrives"
    );

    let (tx, mut rx) = mpsc::channel(64);
    agent.run_turn("two", tx).await.expect("turn ok");
    assert!(
        agent
            .history
            .iter()
            .any(|m| m.text().contains("[Compacted progress summary]")),
        "token threshold compacted the history"
    );
    let (_text, errors, notices) = drain_events(&mut rx);
    assert!(errors.is_empty(), "a successful compaction is not an error");
    assert!(
        notices.iter().any(|n| n.contains("compacted")),
        "compaction surfaced: {notices:?}"
    );
    assert_eq!(
        agent.usage().last_prompt_tokens(),
        None,
        "stale prompt size cleared so compaction does not re-trigger"
    );
    // With last_prompt cleared, context_tokens falls back to a char/4
    // estimate of the remaining history (not the pre-compact 850).
    assert_eq!(
        agent.context_tokens(),
        crate::llm::estimate_history_tokens(agent.history()),
        "post-compact meter uses the remaining-history estimate"
    );

    // The summarization request carried the extended preservation
    // instructions (todo list + plan file).
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    let summarize_prompt = &requests[1].messages[0].text();
    assert!(summarize_prompt.contains("todo"), "{summarize_prompt}");
    assert!(
        summarize_prompt.contains(".wizard/plan.md"),
        "{summarize_prompt}"
    );
}

/// Test tool returning a large fixed blob, to cross the byte threshold
/// mid-turn.
struct BigOutputTool;

#[async_trait::async_trait]
impl crate::tools::Tool for BigOutputTool {
    fn name(&self) -> &str {
        "big"
    }

    fn description(&self) -> &str {
        "Return a large blob (test tool)."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(
        &self,
        _args: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolOutput, crate::tools::ToolError> {
        Ok(ToolOutput::ok("B".repeat(5_000)))
    }
}

/// Compaction is a model call and has to be billed like one. It was not for
/// a long time: the summarizer holds a provider rather than an `Agent`, so
/// its tokens reached no counter and no line of the log, and a run that
/// compacted every few steps under-reported itself by however much that came
/// to. It gets its own record rather than a share of the turn's, because a
/// `/compact` between turns is not inside any turn.
#[tokio::test]
async fn a_compaction_pass_bills_itself_to_the_usage_log() {
    let (mut agent, _provider, tmp) =
        test_agent(vec![vec![usage_chunk("a terse progress note", 4_000, 120)]]);
    for i in 0..(KEEP_RECENT + 5) {
        agent.history.push(ChatMessage::user(format!("msg {i}")));
    }

    let outcome = agent.compact_now().await;
    assert!(
        matches!(outcome, CompactOutcome::Summarized(_)),
        "{outcome:?}"
    );

    let log = std::fs::read_to_string(tmp.0.join("usage.jsonl")).expect("usage log written");
    let record: crate::usage::UsageRecord =
        serde_json::from_str(log.lines().next().expect("one record")).expect("valid json");
    assert_eq!(record.prompt_tokens, 4_000);
    assert_eq!(record.completion_tokens, 120);

    assert_eq!(
        agent.usage().session_totals(),
        (4_000, 120),
        "and `/cost` sees it too"
    );
    assert_eq!(
        agent.usage().turn_totals(),
        (0, 0),
        "but not through the turn counters, which would bill the same tokens a second time"
    );
    assert_eq!(
        agent.usage().last_prompt_tokens(),
        None,
        "the summarizer's prompt is not this conversation's"
    );
}

#[tokio::test]
async fn compact_now_force_summarizes_and_keeps_the_recent_tail() {
    // One scripted response: the summarization call.
    let (mut agent, _provider, _tmp) = test_agent(vec![vec![final_chunk("a terse progress note")]]);
    // history[0] is the system prompt; add a middle span + recent tail.
    let extra = KEEP_RECENT + 5;
    for i in 0..extra {
        agent.history.push(ChatMessage::user(format!("msg {i}")));
    }
    let before = agent.history.len();

    let outcome = agent.compact_now().await;

    assert_eq!(
        outcome,
        CompactOutcome::Summarized(before - 1 - KEEP_RECENT)
    );
    assert!(
        agent
            .history
            .iter()
            .any(|m| m.text().contains("[Compacted progress summary]")),
        "the middle span became a summary note"
    );
    // The system prompt and the last KEEP_RECENT messages survive verbatim.
    assert_eq!(agent.history[0].role, Role::System);
    assert_eq!(
        agent.history.last().unwrap().text(),
        format!("msg {}", extra - 1)
    );
    // Progress note is session-persisted so resume / session readers see it.
    let session = agent.session().load_history().expect("session readable");
    assert!(
        session
            .iter()
            .any(|m| m.role == Role::System && m.text().contains(COMPACT_SUMMARY_HEADING)),
        "compact note must land in the session JSONL as a system note"
    );
}

#[tokio::test]
async fn compact_now_is_a_noop_with_little_history() {
    let (mut agent, _provider, _tmp) = test_agent(vec![]);
    // Only the system prompt plus a couple messages: nothing to compact.
    agent.history.push(ChatMessage::user("hi"));
    let outcome = agent.compact_now().await;
    assert_eq!(outcome, CompactOutcome::Nothing);
}

#[tokio::test]
async fn context_pressure_bands_follow_window_fill() {
    let (agent, _provider, _tmp) = test_agent(vec![]);
    // No window, tiny history → ok via byte proxy.
    let pressure = agent.context_pressure().await;
    assert_eq!(pressure.level, PressureLevel::Ok);
    assert!(pressure.fill < PRESSURE_ELEVATED_FRACTION);

    // Known window + last prompt at 60% → elevated.
    let provider = ScriptedProvider::with_context_window(vec![], 10_000);
    let tmp = TempDir::new();
    let agent = test_agent_with(
        &tmp,
        provider,
        Vec::new(),
        ToolRegistry::with_native_tools(),
    );
    agent.usage.record(Some(6_000), Some(1));
    let pressure = agent.context_pressure().await;
    assert_eq!(pressure.level, PressureLevel::Elevated);
    assert!(pressure.signal_line().contains("elevated"));
    assert!(pressure.signal_line().starts_with(CONTEXT_PRESSURE_HEADING));

    // 75% → high.
    agent.usage.record(Some(7_500), Some(1));
    let pressure = agent.context_pressure().await;
    assert_eq!(pressure.level, PressureLevel::High);

    // 85% → critical (auto-compact band).
    agent.usage.record(Some(8_500), Some(1));
    let pressure = agent.context_pressure().await;
    assert_eq!(pressure.level, PressureLevel::Critical);
    assert!(pressure.signal_line().contains("critical"));
}

#[tokio::test]
async fn byte_threshold_never_trips_critical_when_window_is_known() {
    // Regression: with a large known window, the byte proxy (sized for
    // unknown-window setups) used to force `critical` at a few percent of
    // real fill — nagging "call compact now" while compact_now had nothing
    // to fold. A known window makes the reported prompt authoritative.
    let provider = ScriptedProvider::with_context_window(vec![], 500_000);
    let tmp = TempDir::new();
    let mut agent = test_agent_with(
        &tmp,
        provider,
        Vec::new(),
        ToolRegistry::with_native_tools(),
    );
    agent.config.compact_threshold_bytes = 1_000;
    agent.history.push(ChatMessage::user("x".repeat(10_000)));

    // History bytes far past the threshold, but the reported prompt fills
    // only 7% of the window: pressure must stay ok.
    agent.usage.record(Some(36_600), Some(1));
    let pressure = agent.context_pressure().await;
    assert_eq!(pressure.level, PressureLevel::Ok);
    assert!(pressure.fill < PRESSURE_ELEVATED_FRACTION);

    // And without a reported prompt yet, the char/4 estimate alone must not
    // trip auto-compact either.
    agent.usage.clear_last_prompt();
    let pressure = agent.context_pressure().await;
    assert_ne!(pressure.level, PressureLevel::Critical);
}

#[tokio::test]
async fn compact_tool_runs_mid_turn_and_feeds_result_back() {
    let tmp = TempDir::new();
    let provider = ScriptedProvider::new(vec![
        vec![tool_call_chunk("compact", json!({}))],
        vec![final_chunk("a terse progress note")], // summarization
        vec![final_chunk("done after compact")],
    ]);
    let mut agent = test_agent_with(
        &tmp,
        provider.clone(),
        Vec::new(),
        ToolRegistry::with_native_tools(),
    );
    // Enough history that compact has a middle span.
    for i in 0..(KEEP_RECENT + 5) {
        agent.history.push(ChatMessage::user(format!("old {i}")));
    }

    let (tx, mut rx) = mpsc::channel(64);
    let reason = agent.run_turn("please compact", tx).await.expect("turn ok");
    assert_eq!(reason, DoneReason::Completed);

    // Tool result reached the model as a tool message.
    assert!(
        agent
            .history
            .iter()
            .any(|m| m.role == Role::Tool && m.text().contains("compacted")),
        "compact tool result missing from history"
    );
    assert!(
        agent
            .history
            .iter()
            .any(|m| m.text().contains(COMPACT_SUMMARY_HEADING)),
        "summary note in history"
    );

    // Surfaces saw tool start/finish and a context-size refresh.
    let mut saw_started = false;
    let mut saw_finished = false;
    let mut saw_context = false;
    while let Ok(event) = rx.try_recv() {
        match event {
            AgentEvent::ToolStarted { name, .. } if name == "compact" => saw_started = true,
            AgentEvent::ToolFinished { name, output } if name == "compact" => {
                saw_finished = true;
                assert!(!output.is_error, "{}", output.content);
                assert!(output.content.contains("compacted"), "{}", output.content);
            }
            AgentEvent::ContextSize { .. } => saw_context = true,
            _ => {}
        }
    }
    assert!(saw_started, "ToolStarted for compact");
    assert!(saw_finished, "ToolFinished for compact");
    assert!(saw_context, "ContextSize after compact");
}

#[tokio::test]
async fn elevated_pressure_is_injected_into_the_completion_request() {
    let provider = ScriptedProvider::with_context_window(vec![vec![final_chunk("ok")]], 10_000);
    let tmp = TempDir::new();
    let mut agent = test_agent_with(&tmp, provider.clone(), Vec::new(), ToolRegistry::new());
    // 60% fill → elevated signal on the next completion.
    agent.usage.record(Some(6_000), Some(1));

    let (tx, _rx) = mpsc::channel(8);
    agent.run_turn("hi", tx).await.expect("turn ok");

    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let saw_pressure = requests[0]
        .messages
        .iter()
        .any(|m| m.text().starts_with(CONTEXT_PRESSURE_HEADING));
    assert!(
        saw_pressure,
        "pressure line must ride the completion request"
    );
    drop(requests);

    // Ephemeral: not left in agent history after the step.
    assert!(
        agent
            .history
            .iter()
            .all(|m| !m.text().starts_with(CONTEXT_PRESSURE_HEADING)),
        "pressure must not linger in history"
    );
    // And never session-persisted.
    let session = agent.session().load_history().expect("session");
    assert!(
        session
            .iter()
            .all(|m| !m.text().starts_with(CONTEXT_PRESSURE_HEADING)),
        "pressure must not hit the session file"
    );
}

#[tokio::test]
async fn byte_threshold_compacts_between_steps_keeping_the_turn_tail() {
    let tmp = TempDir::new();
    let provider = ScriptedProvider::new(vec![
        vec![tool_call_chunk("big", json!({}))],
        vec![final_chunk("progress so far")], // compaction summary
        vec![final_chunk("done")],
    ]);
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(BigOutputTool));
    let mut agent = test_agent_with(&tmp, Arc::clone(&provider), Vec::new(), registry);
    for i in 0..13 {
        agent.history.push(ChatMessage::user(format!("filler {i}")));
    }
    // Threshold just above the current size: crossed only once the 5k
    // tool result lands, so the compaction must happen mid-turn.
    let base: usize = agent.history.iter().map(|m| m.text().len()).sum();
    agent.config.compact_threshold_bytes = base + 1_000;

    let (tx, _rx) = mpsc::channel(256);
    let reason = agent.run_turn("go", tx).await.expect("turn ok");
    assert_eq!(reason, DoneReason::Completed);

    assert!(
        agent
            .history
            .iter()
            .any(|m| m.text().contains("[Compacted progress summary]")),
        "mid-turn compaction happened"
    );
    // The in-flight turn's tail — the big tool result the model is
    // reasoning about — survived verbatim.
    assert!(
        agent
            .history
            .iter()
            .any(|m| m.role == Role::Tool && m.text().contains("BBBB")),
        "tool feedback preserved through compaction"
    );
    // The final completion saw the compacted history.
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(
        requests[2]
            .messages
            .iter()
            .any(|m| m.text().contains("[Compacted progress summary]"))
    );
}

#[tokio::test]
async fn todo_writes_update_shared_state_and_emit_events() {
    let tmp = TempDir::new();
    let items = json!([
        { "content": "investigate", "status": "completed" },
        { "content": "implement", "status": "in_progress" },
        { "content": "test", "status": "pending" }
    ]);
    let (mut agent, provider) = test_agent_in(
        &tmp,
        vec![
            vec![tool_call_chunk(
                "todo",
                json!({ "action": "write", "items": items }),
            )],
            vec![final_chunk("noted")],
        ],
        Vec::new(),
        ToolRegistry::with_native_tools(),
    );

    let (tx, mut rx) = mpsc::channel(64);
    agent.run_turn("go", tx).await.expect("turn ok");

    let mut updates = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let AgentEvent::TodoUpdated(items) = event {
            updates.push(items);
        }
    }
    assert_eq!(updates.len(), 1, "one TodoUpdated per write");
    assert_eq!(updates[0].len(), 3);
    assert_eq!(updates[0][1].content, "implement");
    assert_eq!(
        updates[0][1].status,
        crate::tools::todo::TodoStatus::InProgress
    );

    // The shared state holds the list for later read calls.
    assert_eq!(agent.ctx.todos.lock().unwrap().len(), 3);
    let feedback = tool_feedback_of(&provider, 1);
    assert!(feedback.contains("1/3 done"), "{feedback}");
}

#[tokio::test]
async fn todo_tool_stays_usable_in_plan_mode() {
    let tmp = TempDir::new();
    let (mut agent, provider) = test_agent_in(
        &tmp,
        vec![
            vec![tool_call_chunk(
                "todo",
                json!({ "action": "write", "items": [
                    { "content": "draft plan", "status": "in_progress" }
                ] }),
            )],
            vec![final_chunk("planning")],
        ],
        Vec::new(),
        ToolRegistry::with_native_tools(),
    );
    agent.set_plan_mode(true);

    let (tx, _rx) = mpsc::channel(64);
    agent.run_turn("go", tx).await.expect("turn ok");
    let feedback = tool_feedback_of(&provider, 1);
    assert!(
        feedback.contains("todo list updated"),
        "todo runs under the plan gate: {feedback}"
    );
    assert!(agent.plan_mode(), "plan mode untouched");
}

#[test]
fn todo_instruction_appears_only_when_the_tool_is_registered() {
    let tmp = TempDir::new();
    let (agent, _provider) = test_agent_in(
        &tmp,
        Vec::new(),
        Vec::new(),
        ToolRegistry::with_native_tools(),
    );
    assert!(agent.history[0].text().contains("## Working todo list"));

    let (agent, _provider) = test_agent_in(&tmp, Vec::new(), Vec::new(), ToolRegistry::new());
    assert!(
        !agent.history[0].text().contains("## Working todo list"),
        "no instruction without the tool"
    );
}

/// Context stewardship is always on: every agent needs to know how to
/// compact and reset on task change, whether or not `run_command` is in the
/// registry (headless still auto-compacts and can use subagents/memory).
#[test]
fn context_management_instruction_is_always_injected() {
    let tmp = TempDir::new();
    for registry in [ToolRegistry::with_native_tools(), ToolRegistry::new()] {
        let (agent, _provider) = test_agent_in(&tmp, Vec::new(), Vec::new(), registry);
        assert!(
            agent.history[0]
                .text()
                .contains("## Context management (you own your window)"),
            "context block missing from system prompt"
        );
        assert!(
            agent.history[0].text().contains("`compact`"),
            "must teach the compact tool"
        );
        assert!(
            agent.history[0].text().contains("[context pressure]"),
            "must mention the live pressure signal"
        );
    }
}

#[tokio::test]
async fn background_task_finish_is_injected_into_the_next_step() {
    use crate::tools::tasks::TaskStatus;

    let tmp = TempDir::new();
    let (mut agent, provider) = test_agent_in(
        &tmp,
        vec![
            // Turn 1: start a background task, then stop.
            vec![tool_call_chunk(
                "execute",
                json!({ "command": "echo task-payload", "run_in_background": true }),
            )],
            vec![final_chunk("started it")],
            // Turn 2: plain reply (the notification precedes it).
            vec![final_chunk("noted the finish")],
        ],
        Vec::new(),
        ToolRegistry::with_native_tools(),
    );

    let (tx, _rx) = mpsc::channel(64);
    agent
        .run_turn("run it in the background", tx)
        .await
        .expect("turn ok");
    let feedback = tool_feedback_of(&provider, 1);
    assert!(
        feedback.contains("Background task #1 started: echo task-payload"),
        "spawn returns immediately with the id: {feedback}"
    );

    // Wait for the echo to actually finish in the registry.
    let deadline = Instant::now() + Duration::from_secs(10);
    while agent.ctx.tasks.status(1) == Some(TaskStatus::Running) {
        assert!(Instant::now() < deadline, "background task finished");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let (tx, mut rx) = mpsc::channel(64);
    agent.run_turn("anything new?", tx).await.expect("turn ok");

    // The next step's request carried the finished-task notification
    // (with the output tail) ahead of the model call.
    {
        let requests = provider.requests.lock().unwrap();
        let request = requests.last().expect("turn 2 request");
        let note = request
            .messages
            .iter()
            .find(|m| m.role == Role::System && m.text().contains("background task #1 finished"))
            .expect("notification in history");
        assert!(note.text().contains("(exit 0)"), "{}", note.text());
        assert!(
            note.text().contains("task-payload"),
            "output tail included: {}",
            note.text()
        );
    }

    // The surfaces saw a TaskFinished event.
    let mut finished = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let AgentEvent::TaskFinished {
            id,
            command,
            status,
        } = event
        {
            finished.push((id, command, status));
        }
    }
    assert_eq!(
        finished,
        [(1, "echo task-payload".to_string(), TaskStatus::Done(0))]
    );

    // Drained exactly once: nothing left for later steps.
    assert!(agent.ctx.tasks.drain_completed().is_empty());
    assert_eq!(
        agent
            .history()
            .iter()
            .filter(|m| m.text().contains("background task #1 finished"))
            .count(),
        1,
        "the notification appears exactly once in history"
    );
}

#[tokio::test]
async fn hook_timeout_does_not_hang_the_turn() {
    let tmp = TempDir::new();
    let command = write_script(&tmp.0, "slow.sh", "sleep 5\n");
    let (registry, calls) = recording_registry();
    let (mut agent, _provider) = test_agent_in(
        &tmp,
        vec![
            vec![tool_call_chunk("echo", json!({}))],
            vec![final_chunk("done")],
        ],
        vec![HookDef {
            event: HookEvent::PreToolUse,
            matcher: None,
            command,
            timeout_secs: Some(1),
        }],
        registry,
    );

    let started = Instant::now();
    let (tx, _rx) = mpsc::channel(64);
    let reason = agent.run_turn("go", tx).await.expect("turn ok");
    assert_eq!(reason, DoneReason::Completed);
    assert_eq!(calls.lock().unwrap().len(), 1, "the tool still ran");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "the hook was killed at its 1s timeout (took {:?})",
        started.elapsed()
    );
}

#[tokio::test]
async fn stream_json_sink_renders_a_scripted_run_as_jsonl_ending_in_done() {
    use crate::output::{EventSink, StreamJsonSink, tests::SharedBuf};

    let tmp = TempDir::new();
    let (registry, _calls) = recording_registry();
    let (mut agent, _provider) = test_agent_in(
        &tmp,
        vec![
            vec![tool_call_chunk("echo", json!({ "text": "hi" }))],
            vec![usage_chunk("all wrapped up", 42, 7)],
        ],
        Vec::new(),
        registry,
    );

    let (tx, mut rx) = mpsc::channel(256);
    let reason = agent.run_turn("go", tx).await.expect("turn ok");
    assert_eq!(reason, DoneReason::Completed);

    // Feed the turn's real event stream through the stream-json sink,
    // exactly as the headless runner wires it.
    let buf = SharedBuf::default();
    let mut sink = StreamJsonSink::new(buf.clone());
    while let Ok(event) = rx.try_recv() {
        sink.event(event);
    }
    sink.finish(reason);

    let out = buf.contents();
    let values: Vec<serde_json::Value> = out
        .lines()
        .map(|line| serde_json::from_str(line).expect("every line is valid JSON"))
        .collect();
    assert!(values.len() >= 4, "got: {out}");
    let types: Vec<&str> = values
        .iter()
        .filter_map(|value| value["type"].as_str())
        .collect();
    assert!(types.contains(&"tool_call"), "got: {types:?}");
    assert!(types.contains(&"tool_result"), "got: {types:?}");
    assert!(types.contains(&"text_delta"), "got: {types:?}");
    assert!(types.contains(&"usage"), "got: {types:?}");
    let done = values.last().expect("at least the done line");
    assert_eq!(done["type"], "done");
    assert_eq!(done["reason"], "completed");
    assert_eq!(done["usage"]["prompt_tokens"], 42);
    assert_eq!(done["usage"]["completion_tokens"], 7);
}

#[tokio::test]
async fn spawn_subagent_background_returns_immediately_and_reports_on_a_later_turn() {
    let tmp = TempDir::new();

    // The subagent gets its own scripted provider so its chat_stream
    // calls can't race the parent's — they're decoupled queues.
    let sub_provider = ScriptedProvider::new(vec![vec![final_chunk("found the answer")]]);
    let sub_hooks = Arc::new(HookEngine::new(
        Vec::new(),
        tmp.0.clone(),
        "sub-session".to_string(),
    ));
    let spawn_tool = subagent::SpawnSubagentTool::new(
        subagent::builtin_configs(),
        sub_provider,
        Arc::new(ToolRegistry::new()),
        sub_hooks,
    );
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(spawn_tool));

    let (mut agent, provider) = test_agent_in(
        &tmp,
        vec![
            vec![tool_call_chunk(
                "spawn_subagent",
                json!({"subagent": "worker", "task": "investigate X", "background": true}),
            )],
            vec![final_chunk("kicked it off, anything else?")],
            // Second turn's response, below.
            vec![final_chunk("got it")],
        ],
        Vec::new(),
        registry,
    );

    let (tx, mut rx) = mpsc::channel(64);
    let reason = agent.run_turn("delegate this", tx).await.expect("turn ok");
    assert_eq!(reason, DoneReason::Completed);

    // The turn did not wait on the subagent: both of the parent's
    // scripted responses were already consumed.
    assert_eq!(provider.requests.lock().unwrap().len(), 2);

    let mut started = None;
    let mut tool_result = None;
    while let Ok(event) = rx.try_recv() {
        match event {
            AgentEvent::SubagentStarted { id, name, task } => {
                started = Some((id, name, task));
            }
            AgentEvent::ToolFinished { name, output } if name == "spawn_subagent" => {
                tool_result = Some(output);
            }
            _ => {}
        }
    }
    let (id, name, task) = started.expect("SubagentStarted was emitted");
    assert_eq!(id, 1);
    assert_eq!(name, "worker");
    assert_eq!(task, "investigate X");
    let tool_result = tool_result.expect("spawn_subagent's tool result was observed");
    assert!(!tool_result.is_error);
    assert!(
        tool_result.content.contains("Running in the background"),
        "{}",
        tool_result.content
    );

    // Let the detached subagent actually finish before the next turn.
    let deadline = Instant::now() + Duration::from_secs(10);
    while agent.ctx.subagents.pending_count() > 0 {
        assert!(
            Instant::now() < deadline,
            "background subagent did not finish in time"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // A follow-up turn's top-of-loop drain picks up the report: it's
    // injected into history and surfaced as SubagentFinished, without
    // the model ever having to ask for it.
    let (tx2, mut rx2) = mpsc::channel(64);
    agent
        .run_turn("anything happen?", tx2)
        .await
        .expect("second turn ok");

    let mut finished = None;
    while let Ok(event) = rx2.try_recv() {
        if let AgentEvent::SubagentFinished {
            id,
            name,
            completed,
            output,
            ..
        } = event
        {
            finished = Some((id, name, completed, output));
        }
    }
    let (id, name, completed, output) = finished.expect("SubagentFinished was emitted");
    assert_eq!(id, 1);
    assert_eq!(name, "worker");
    assert!(completed);
    assert_eq!(output, "found the answer");

    assert_eq!(
        agent
            .history()
            .iter()
            .filter(|m| m
                .text()
                .contains("background subagent #1 'worker' completed"))
            .count(),
        1,
        "the report appears exactly once in history"
    );
}

#[test]
fn error_classification_prefers_typed_provider_errors() {
    let permanent: anyhow::Error = crate::llm::ProviderError::http(401, "bad key").into();
    assert!(!error_is_transient(&permanent));
    let rate_limited: anyhow::Error = crate::llm::ProviderError::http(429, "slow down").into();
    assert!(error_is_transient(&rate_limited));
    let server: anyhow::Error = crate::llm::ProviderError::http(500, "oops").into();
    assert!(error_is_transient(&server));
    let transport: anyhow::Error = crate::llm::ProviderError::transport("reset").into();
    assert!(error_is_transient(&transport));
    // Context wrapping must not hide the classification.
    let wrapped = permanent.context("starting chat completion");
    assert!(!error_is_transient(&wrapped));
    // Unknown errors stay transient for robustness.
    assert!(error_is_transient(&anyhow::anyhow!("mid-stream drop")));
}

#[test]
fn failed_background_subagents_are_labeled_failed() {
    let note = subagent_note(&crate::tools::subagent_tasks::SubagentTaskResult {
        id: 3,
        name: "worker".to_string(),
        task: "doomed".to_string(),
        completed: false,
        output: "subagent failed: connection refused".to_string(),
        steps_used: 0,
        error: Some("connection refused".to_string()),
    });
    assert!(
        note.contains("'worker' failed: connection refused after 0 step(s)"),
        "{note}"
    );
    assert!(
        !note.contains("step budget"),
        "a hard error is not a budget stop: {note}"
    );
}

/// Test tool that fires the agent's cancel handle when executed, to
/// exercise mid-batch interruption deterministically.
struct CancelingTool {
    handle: Arc<Mutex<Option<CancelHandle>>>,
}

#[async_trait::async_trait]
impl crate::tools::Tool for CancelingTool {
    fn name(&self) -> &str {
        "cancel_me"
    }

    fn description(&self) -> &str {
        "Cancel the turn (test tool)."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(
        &self,
        _args: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolOutput, crate::tools::ToolError> {
        self.handle
            .lock()
            .unwrap()
            .as_ref()
            .expect("handle bound")
            .cancel();
        Ok(ToolOutput::ok("cancelling"))
    }
}

/// `done: true` chunk carrying several tool calls in one batch.
fn multi_tool_chunk(names: &[&str]) -> ChatChunk {
    ChatChunk {
        message: Some(ChatMessage::assistant_turn(
            "",
            Vec::new(),
            names
                .iter()
                .map(|name| ToolCall::new(name.to_string(), json!({})))
                .collect(),
        )),
        images: Vec::new(),
        thinking: false,
        done: true,
        done_reason: None,
        eval_count: None,
        prompt_eval_count: None,
        cache: CacheTokens::NONE,
    }
}

#[tokio::test]
async fn cancel_mid_batch_stops_the_turn_and_answers_skipped_calls() {
    let tmp = TempDir::new();
    let handle_slot = Arc::new(Mutex::new(None));
    let (mut registry, echo_calls) = recording_registry();
    registry.register(Arc::new(CancelingTool {
        handle: Arc::clone(&handle_slot),
    }));
    let (mut agent, provider) = test_agent_in(
        &tmp,
        // One completion: a two-call batch. The turn must stop before
        // asking for another.
        vec![vec![multi_tool_chunk(&["cancel_me", "echo"])]],
        Vec::new(),
        registry,
    );
    *handle_slot.lock().unwrap() = Some(agent.cancel_handle());

    let (tx, _rx) = mpsc::channel(64);
    let reason = agent.run_turn("go", tx).await.expect("turn ok");
    assert_eq!(reason, DoneReason::Stopped);
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    assert!(
        echo_calls.lock().unwrap().is_empty(),
        "the call after the cancel never ran"
    );

    // Both tool calls are answered (the second synthetically) on ONE tool
    // message, so the persisted history carries no dangling tool_use and no
    // batch split across two messages.
    let persisted = agent.session().load_history().expect("session readable");
    let assistant = persisted
        .iter()
        .position(|m| m.role == Role::Assistant && m.tool_calls().len() == 2)
        .expect("assistant batch persisted");
    let calls = persisted[assistant].tool_calls();
    assert_eq!(persisted[assistant + 1].role, Role::Tool);
    assert_eq!(
        persisted.len(),
        assistant + 2,
        "one message answers the whole batch"
    );
    let results = persisted[assistant + 1].tool_results();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].tool_use_id, calls[0].id);
    assert_eq!(results[1].tool_use_id, calls[1].id);
    assert_eq!(results[1].name, "echo");
    assert!(
        results[1].content.contains("interrupted by user"),
        "{}",
        results[1].content
    );

    // The next turn is not poisoned by the stale cancel request.
    assert!(agent.cancel_handle().is_cancelled());
    agent.cancel.clear();
    assert!(!agent.cancel_handle().is_cancelled());
}

#[tokio::test]
async fn compaction_never_splits_a_tool_call_group() {
    // One scripted response: the summarization call.
    let (mut agent, _provider, _tmp) = test_agent(vec![vec![final_chunk("a terse progress note")]]);
    // Arrange history so the naive cut (len - KEEP_RECENT) would land on
    // a tool result, splitting it from its assistant tool-call message.
    for i in 0..4 {
        agent.history.push(ChatMessage::user(format!("filler {i}")));
    }
    let mut assistant = ChatMessage::assistant("running a tool");
    assistant.push_tool_call(ToolCall::new("execute".to_string(), json!({})));
    agent.history.push(assistant); // index 5
    agent.history.push(ChatMessage::tool_result(
        "call_execute",
        "execute",
        "output",
    )); // index 6
    for i in 0..9 {
        agent.history.push(ChatMessage::user(format!("tail {i}")));
    }
    assert_eq!(agent.history.len(), 16, "naive cut would be index 6");

    let outcome = agent.compact_now().await;
    // Snapped back to the assistant at index 5, not past it onto the user:
    // the tool-call group stays the tail opener and 4 messages went.
    assert_eq!(outcome, CompactOutcome::Summarized(4));
    let assistant = agent
        .history
        .iter()
        .position(|m| !m.tool_calls().is_empty())
        .expect("tool-call message survived");
    assert_eq!(
        agent.history[assistant + 1].role,
        Role::Tool,
        "the tool call kept its result"
    );
}

#[tokio::test]
async fn drain_finished_notifications_reports_and_persists_once() {
    let (mut agent, _provider, _tmp) = test_agent(vec![]);
    agent.ctx.subagents.spawn("worker", "doomed", async {
        crate::tools::subagent_tasks::SubagentRunResult {
            completed: false,
            output: "subagent failed: boom".to_string(),
            steps_used: 0,
            error: Some("boom".to_string()),
        }
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while agent.ctx.subagents.pending_count() > 0 {
        assert!(Instant::now() < deadline, "background subagent finished");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let notifications = agent.drain_finished_notifications();
    assert_eq!(notifications.len(), 1);
    match &notifications[0] {
        FinishedNotification::Subagent(task) => {
            assert_eq!(task.error.as_deref(), Some("boom"));
        }
        other => panic!("expected a subagent notification, got {other:?}"),
    }
    let note = agent.history().last().expect("note in history");
    assert_eq!(note.role, Role::System);
    assert!(note.text().contains("failed: boom"), "{}", note.text());

    // Persisted as a system note: a resume replays it.
    let replayed = agent.session().load_history().expect("session readable");
    assert!(
        replayed
            .iter()
            .any(|m| m.role == Role::System && m.text().contains("failed: boom")),
        "note persisted for resume"
    );

    assert!(
        agent.drain_finished_notifications().is_empty(),
        "each finish is reported exactly once"
    );
}

#[tokio::test]
async fn side_question_answers_without_touching_history_or_session() {
    let (agent, provider, _tmp) = test_agent(vec![vec![final_chunk("forty-two")]]);
    let before = agent.history().len();
    let session_bytes_before = std::fs::metadata(agent.session().path())
        .map(|m| m.len())
        .unwrap_or(0);

    let answer = agent
        .answer_side_question("what is 6 * 7?")
        .await
        .expect("side question answers");
    assert_eq!(answer, "forty-two");

    // History and the session file are untouched — that is the whole point.
    assert_eq!(agent.history().len(), before, "history unchanged");
    let session_bytes_after = std::fs::metadata(agent.session().path())
        .map(|m| m.len())
        .unwrap_or(0);
    assert_eq!(
        session_bytes_after, session_bytes_before,
        "session file unchanged"
    );

    // The forked call carried no tools and included the conversation.
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].tools.is_empty(), "side questions are tool-less");
    assert!(
        requests[0]
            .messages
            .iter()
            .any(|m| m.role == Role::User && m.text().contains("what is 6 * 7?")),
        "question reached the model"
    );
    assert!(
        requests[0]
            .messages
            .iter()
            .any(|m| m.role == Role::User && m.text().contains("NO tools")),
        "system reminder constrains the model"
    );
}

#[tokio::test]
async fn clear_kills_background_work_and_resets_todos() {
    let (mut agent, _provider, _tmp) = test_agent(vec![]);
    agent
        .ctx
        .todos
        .lock()
        .unwrap()
        .push(crate::tools::todo::TodoItem {
            content: "stale item".to_string(),
            status: crate::tools::todo::TodoStatus::Pending,
        });
    agent.ctx.subagents.spawn("worker", "slow", async {
        tokio::time::sleep(Duration::from_secs(30)).await;
        crate::tools::subagent_tasks::SubagentRunResult {
            completed: true,
            output: "never".to_string(),
            steps_used: 1,
            error: None,
        }
    });
    let old_session = agent.session().path().to_path_buf();

    agent.clear().expect("clear ok");
    let new_session_path = agent.session().path().to_path_buf();

    assert_ne!(new_session_path, old_session, "fresh session file");
    assert!(agent.ctx.todos.lock().unwrap().is_empty(), "todos reset");
    assert_eq!(agent.ctx.subagents.pending_count(), 0);
    assert!(
        agent.ctx.subagents.list().is_empty(),
        "old subagents detached"
    );
    assert!(agent.ctx.tasks.list().is_empty(), "old tasks detached");
    assert!(
        agent.drain_finished_notifications().is_empty(),
        "nothing from the old conversation leaks into the new one"
    );
    assert_eq!(
        agent.usage().session_totals(),
        (0, 0),
        "session token counters zeroed with the wiped conversation"
    );
    // context_tokens falls back to an estimate of the remaining system
    // prompt (history was truncated to 1).
    assert!(
        agent.context_tokens() > 0,
        "post-clear meter reflects the system prompt only"
    );

    // The real sessions dir was touched: clean up the empty file.
    let _ = std::fs::remove_file(new_session_path);
}

#[tokio::test]
async fn default_budget_runs_past_the_old_step_ceiling() {
    // 30 tool-calling steps, then a final answer. The budget used to stop
    // this turn at 25; unlimited (the default) carries it to the end.
    let tmp = TempDir::new();
    let (registry, calls) = recording_registry();
    let mut responses: Vec<Vec<ChatChunk>> = (0..30)
        .map(|_| vec![tool_call_chunk("echo", json!({}))])
        .collect();
    responses.push(vec![final_chunk("done")]);
    let (mut agent, _provider) = test_agent_in(&tmp, responses, Vec::new(), registry);
    assert_eq!(agent.config.max_steps, StepBudget::UNLIMITED);

    let (tx, _rx) = mpsc::channel(256);
    let reason = agent.run_turn("go", tx).await.expect("turn ok");

    assert_eq!(reason, DoneReason::Completed);
    assert_eq!(calls.lock().unwrap().len(), 30, "every step ran");
}

#[tokio::test]
async fn configured_cap_still_ends_the_turn() {
    let tmp = TempDir::new();
    let (registry, calls) = recording_registry();
    // More tool calls than the cap allows: the loop must stop at the cap.
    let responses: Vec<Vec<ChatChunk>> = (0..3)
        .map(|_| vec![tool_call_chunk("echo", json!({}))])
        .collect();
    let (mut agent, _provider) = test_agent_in(&tmp, responses, Vec::new(), registry);
    agent.config.max_steps = StepBudget::new(3);

    let (tx, _rx) = mpsc::channel(256);
    let reason = agent.run_turn("go", tx).await.expect("turn ok");

    assert_eq!(reason, DoneReason::MaxSteps);
    assert_eq!(calls.lock().unwrap().len(), 3, "stopped at the cap");
}

/// Streaming (not-done) chunk carrying a text or thinking delta.
fn delta_chunk(content: &str, thinking: bool) -> ChatChunk {
    ChatChunk {
        message: Some(ChatMessage::assistant(content)),
        images: Vec::new(),
        thinking,
        done: false,
        done_reason: None,
        eval_count: None,
        prompt_eval_count: None,
        cache: CacheTokens::NONE,
    }
}

#[tokio::test]
async fn streaming_assembles_split_deltas_and_keeps_thinking_out_of_history() {
    let (mut agent, _provider, _tmp) = test_agent(vec![vec![
        delta_chunk("pondering deeply", true),
        delta_chunk("Hel", false),
        delta_chunk("lo world", false),
        final_chunk(""),
    ]]);

    let (tx, mut rx) = mpsc::channel(64);
    let reason = agent.run_turn("hi", tx).await.expect("turn ok");
    assert_eq!(reason, DoneReason::Completed);

    let mut text = String::new();
    let mut thinking = String::new();
    while let Ok(event) = rx.try_recv() {
        match event {
            AgentEvent::TextDelta(delta) => text.push_str(&delta),
            AgentEvent::ThinkingDelta(delta) => thinking.push_str(&delta),
            _ => {}
        }
    }
    assert_eq!(text, "Hello world", "split deltas reassemble in order");
    assert_eq!(thinking, "pondering deeply", "reasoning is surfaced");

    let assistant = agent
        .history()
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .expect("assistant message");
    assert_eq!(assistant.text(), "Hello world");
    let persisted = agent.session().load_messages().expect("session readable");
    assert!(
        persisted.iter().all(|m| !m.text().contains("pondering")),
        "thinking never reaches history or disk"
    );
}

#[tokio::test]
async fn a_transient_stream_failure_emits_stream_retrying_then_recovers() {
    let tmp = TempDir::new();
    let provider = ScriptedProvider::flaky(1, vec![vec![final_chunk("second try")]]);
    let mut agent = test_agent_with(&tmp, Arc::clone(&provider), Vec::new(), ToolRegistry::new());
    agent.config.retry_base_secs = 0;
    agent.config.retry_max_secs = 0;

    let (tx, mut rx) = mpsc::channel(256);
    let reason = agent.run_turn("go", tx).await.expect("turn ok");
    assert_eq!(reason, DoneReason::Completed);
    assert_eq!(provider.requests.lock().unwrap().len(), 2, "retried once");

    let mut retrying = 0;
    let mut text = String::new();
    let mut errors = Vec::new();
    while let Ok(event) = rx.try_recv() {
        match event {
            AgentEvent::StreamRetrying => retrying += 1,
            AgentEvent::TextDelta(delta) => text.push_str(&delta),
            AgentEvent::Error(message) => errors.push(message),
            _ => {}
        }
    }
    assert_eq!(
        retrying, 1,
        "consumers are told to drop their partial buffer exactly once"
    );
    assert_eq!(text, "second try");
    assert!(
        errors.iter().any(|e| e.contains("retrying")),
        "the outage is surfaced: {errors:?}"
    );
    assert!(
        !agent.llm_breaker.is_open(),
        "one flake never trips the breaker"
    );
}

#[tokio::test]
async fn a_failed_summary_falls_back_to_truncating_the_middle() {
    // The summarization call streams an empty reply, which counts as a
    // summary failure — the middle span is dropped instead.
    let (mut agent, _provider, _tmp) = test_agent(vec![vec![final_chunk("")]]);
    let extra = KEEP_RECENT + 5;
    for i in 0..extra {
        agent.history.push(ChatMessage::user(format!("msg {i}")));
    }
    let before = agent.history.len();

    let outcome = agent.compact_now().await;
    match &outcome {
        CompactOutcome::Truncated { count, error } => {
            assert_eq!(*count, before - 1 - KEEP_RECENT);
            assert!(error.contains("empty summary"), "{error}");
        }
        other => panic!("expected truncation, got {other:?}"),
    }
    assert!(outcome.describe().contains("truncation"));

    assert_eq!(agent.history.len(), 1 + KEEP_RECENT);
    assert!(
        agent
            .history
            .iter()
            .all(|m| !m.text().contains("[Compacted progress summary]")),
        "no summary note on the fallback path"
    );
    assert_eq!(agent.history[0].role, Role::System);
    assert_eq!(
        agent.history.last().unwrap().text(),
        format!("msg {}", extra - 1),
        "the recent tail survives verbatim"
    );
    assert_eq!(
        agent.usage().last_prompt_tokens(),
        None,
        "stale prompt size cleared even on the fallback path"
    );
}

#[tokio::test]
async fn rolling_summarization_chains_oversized_spans_through_chunk_summaries() {
    let (mut agent, provider, _tmp) = test_agent(vec![
        vec![final_chunk("summary of part one")],
        vec![final_chunk("summary of everything")],
    ]);
    // One middle message larger than a single summarization chunk, made of
    // multibyte characters so the split must respect char boundaries.
    agent.history.push(ChatMessage::user("é".repeat(15_000)));
    for i in 0..KEEP_RECENT {
        agent.history.push(ChatMessage::user(format!("tail {i}")));
    }

    let outcome = agent.compact_now().await;
    assert_eq!(outcome, CompactOutcome::Summarized(1));

    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "one summarization pass per chunk");
    let second_blob = &requests[1].messages[1].text();
    assert!(
        second_blob.contains("[Progress summary of the transcript so far]"),
        "the second pass sees the first pass's summary"
    );
    assert!(second_blob.contains("summary of part one"));
    assert!(second_blob.contains("[Transcript continues]"));
    drop(requests);

    assert!(
        agent.history.iter().any(|m| m
            .text()
            .contains("[Compacted progress summary]\nsummary of everything")),
        "the final rolling summary is what lands in history"
    );
}

#[test]
fn leaving_plan_mode_also_leaves_omakase() {
    let tmp = TempDir::new();
    let (mut agent, _provider) = test_agent_in(&tmp, Vec::new(), Vec::new(), ToolRegistry::new());

    agent.set_omakase(true);
    assert!(agent.plan_mode(), "omakase implies plan mode");
    assert!(agent.omakase());
    assert!(agent.history[0].text().contains("Omakase"));
    assert!(agent.history[0].text().contains("PLAN MODE"));

    agent.set_plan_mode(false);
    assert!(!agent.omakase(), "no omakase without the plan phase");
    assert!(!agent.history[0].text().contains("Omakase"));
    assert!(!agent.history[0].text().contains("PLAN MODE"));
}

/// A one-lens, no-judge ultra engine. One candidate makes the scripted
/// provider's queue deterministic: the pre-phase takes exactly one response
/// (the draft), the main loop the next.
fn ultra_engine() -> Arc<ultra::UltraEngine> {
    Arc::new(ultra::UltraEngine {
        lenses: vec![subagent::SubagentConfig {
            name: "implementer".to_string(),
            description: "drafts".to_string(),
            system_prompt: "draft it".to_string(),
            tool_scope: None,
            max_steps: StepBudget::new(1),
        }],
        judge: ultra::builtin_judge(),
        judges: 0,
        timeout: Duration::from_secs(30),
        max_draft_chars: 6_000,
        // No panel seats: these tests run ultra on its own, which is the
        // council falling back to the session's own client and model.
        seats: Vec::new(),
    })
}

#[tokio::test]
async fn ultra_guidance_lives_for_one_turn_and_is_never_persisted() {
    let tmp = TempDir::new();
    let provider = ScriptedProvider::new(vec![
        vec![final_chunk("draft: rename the flag in cli.rs")], // turn 1 candidate
        vec![final_chunk("renamed it")],                       // turn 1 main loop
        vec![final_chunk("draft: and the docs too")],          // turn 2 candidate
        vec![final_chunk("done")],                             // turn 2 main loop
    ]);
    let mut agent = test_agent_with(&tmp, provider.clone(), Vec::new(), ToolRegistry::new());
    agent.set_ultra(Some(ultra_engine()));

    let (tx, mut rx) = mpsc::channel(64);
    agent.run_turn("rename the flag", tx.clone()).await.unwrap();

    // The drafts reached the model that acts on them...
    let main_turn = provider.requests.lock().unwrap()[1].clone();
    let injected: Vec<&ChatMessage> = main_turn
        .messages
        .iter()
        .filter(|message| ultra::is_guidance(message))
        .collect();
    assert_eq!(injected.len(), 1, "exactly one guidance block");
    assert!(injected[0].text().contains("rename the flag in cli.rs"));

    // ...and the surface got them too, or they would be readable nowhere:
    // the candidate's pane retires within seconds and a system message is
    // never rendered in the transcript.
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::UltraGuidance { guidance, .. }
                if guidance.contains("rename the flag in cli.rs")
        )),
        "the drafts are surfaced for the user to keep"
    );

    // The turn is over, so the advice about it is over.
    assert!(
        !agent.history.iter().any(ultra::is_guidance),
        "guidance is turn-scoped: left in, one block per ultra turn accumulates in the window \
         and every later turn re-sends drafts about requests that were already answered"
    );
    assert!(
        !agent
            .session()
            .load_messages()
            .expect("session loads")
            .iter()
            .any(ultra::is_guidance),
        "and it is not in the session either, so /resume does not bring it back"
    );

    // A second ultra turn sees its own drafts and none of the last turn's.
    agent.run_turn("now the docs", tx).await.unwrap();
    let second_turn = provider.requests.lock().unwrap()[3].clone();
    let injected: Vec<&ChatMessage> = second_turn
        .messages
        .iter()
        .filter(|message| ultra::is_guidance(message))
        .collect();
    assert_eq!(injected.len(), 1, "still exactly one, not two");
    assert!(injected[0].text().contains("and the docs too"));
    assert!(
        !injected[0].text().contains("rename the flag in cli.rs"),
        "last turn's drafts are gone"
    );
}

/// Provider that raises the parent's cancel handle as soon as it is asked
/// for a completion, and then never answers — the interrupt that arrives
/// while the ultra fan-out is mid-stream, which is when a user is most
/// likely to press Ctrl-C (nothing streams during the pre-phase).
struct CancelOnCallProvider {
    handle: Arc<Mutex<Option<CancelHandle>>>,
}

#[async_trait::async_trait]
impl LlmProvider for CancelOnCallProvider {
    async fn health(&self) -> Result<()> {
        Ok(())
    }

    async fn supports_native_tools(&self, _model: &str) -> Result<bool> {
        Ok(true)
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    async fn chat_stream(&self, _request: ChatRequest) -> Result<crate::llm::ChatStream> {
        self.handle
            .lock()
            .unwrap()
            .as_ref()
            .expect("handle bound")
            .cancel();
        // Never answers: the run can only end by being cancelled.
        tokio::time::sleep(Duration::from_secs(3_600)).await;
        unreachable!("the cancelled run is dropped long before this")
    }

    async fn context_window(&self, _model: &str) -> Option<u32> {
        None
    }

    fn label(&self) -> String {
        "cancel-on-call".to_string()
    }
}

#[tokio::test]
async fn cancelling_the_turn_stops_the_ultra_fanout_and_closes_its_panes() {
    let tmp = TempDir::new();
    let slot = Arc::new(Mutex::new(None));
    let provider = Arc::new(CancelOnCallProvider {
        handle: Arc::clone(&slot),
    });
    let session = Session::create(&tmp.0).expect("create session");
    let hooks = Arc::new(HookEngine::new(
        Vec::new(),
        tmp.0.clone(),
        session.id.clone(),
    ));
    let mut agent = Agent::new(
        provider,
        ToolRegistry::new(),
        Config::default(),
        Vec::new(),
        tmp.0.clone(),
        session,
        true,
        hooks,
    )
    .expect("build agent");
    agent.set_usage_log(None);
    agent.set_ultra(Some(ultra_engine()));
    // Exactly what the TUI does before it hands the agent to the turn task:
    // it keeps this handle, and Ctrl-C raises it (see `AppAction::Interrupt`).
    *slot.lock().unwrap() = Some(agent.cancel_handle());

    let (tx, mut rx) = mpsc::channel(64);
    let reason = agent
        .run_turn("something slow", tx)
        .await
        .expect("no error");
    assert_eq!(
        reason,
        DoneReason::Stopped,
        "the turn ends, and ends stopped"
    );

    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    let opened: Vec<u64> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::SubagentRunStarted { run, .. } => Some(*run),
            _ => None,
        })
        .collect();
    assert_eq!(opened.len(), 1, "the candidate's pane opened");
    let closed: Vec<(u64, bool)> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::SubagentRunDone { run, completed, .. } => Some((*run, *completed)),
            _ => None,
        })
        .collect();
    assert_eq!(
        closed,
        [(opened[0], false)],
        "and it was closed out on the way through: a pane the fan-out leaves at 'running' \
         never retires off the rail, because retirement keys off `finished`"
    );
    assert!(
        !agent.history.iter().any(ultra::is_guidance),
        "a cancelled pre-phase injects nothing"
    );
}

#[tokio::test]
async fn ultra_candidates_bill_the_parents_usage_log() {
    let tmp = TempDir::new();
    let provider = ScriptedProvider::new(vec![
        vec![usage_chunk("draft: do it", 500, 80)], // the candidate
        vec![usage_chunk("did it", 200, 30)],       // the main loop
    ]);
    let mut agent = test_agent_with(&tmp, provider, Vec::new(), ToolRegistry::new());
    agent.set_ultra(Some(ultra_engine()));

    let (tx, _rx) = mpsc::channel(64);
    agent.run_turn("do it", tx).await.unwrap();

    assert_eq!(
        agent.usage().turn_totals(),
        (700, 110),
        "the candidate's tokens are the turn's tokens — an ultra turn that reported only the \
         main agent's spend would understate itself several times over, under a chip that \
         advertises exactly that multiplier"
    );
    let log = std::fs::read_to_string(tmp.0.join("usage.jsonl")).expect("usage log written");
    assert!(log.contains("\"prompt_tokens\":700"), "{log}");
}

// ---------------------------------------------------------------------------
// The data event: cloning, recording, replay
// ---------------------------------------------------------------------------

/// Every shape the event stream carries has to survive being written down and
/// read back, because that is the whole reason the reply channels moved off it:
/// a recording, a replay and (later) a peer all go through exactly this path.
/// The structured payloads are the ones that break: a tool result, an image
/// reference, a hook outcome, a task status, a gate ticket. Those are what
/// this pins.
/// The report/request split, and specifically the half of it that is easy to
/// get backwards.
///
/// A gate-bearing event *looks* like a request: something is waiting for an
/// answer. It is still a report, because the question is a fact about the
/// sender's turn and a watcher should see it; what a watcher must not get is
/// the ability to answer, and that is taken away by voiding the ticket rather
/// than by refusing to carry the event. Getting this backwards does not fail
/// any other test: it just silently stops peers from seeing plan reviews.
#[test]
fn a_gate_is_a_report_and_a_command_line_is_a_request() {
    let (gate, _verdict) = PlanGate::open();
    let (interview, _answers) = InterviewGate::open();

    assert!(
        AgentEvent::CommandRequested("/model gpt-5.3-codex".into()).is_request(),
        "a slash-command line asks this machine to act"
    );
    assert!(
        !AgentEvent::PlanReady {
            plan: "1. read\n2. write".into(),
            gate,
        }
        .is_request(),
        "a plan review is a fact about the sender's turn"
    );
    assert!(
        !AgentEvent::Interview {
            questions: Vec::new(),
            gate: interview,
        }
        .is_request(),
        "an interview is a fact about the sender's turn"
    );
    assert!(!AgentEvent::TextDelta("hello".into()).is_request());
    assert!(!AgentEvent::StreamRetrying.is_request());
}

#[test]
fn agent_events_round_trip_through_serde() {
    let (gate, _verdict) = PlanGate::open();
    let (interview, _answers) = InterviewGate::open();
    let events = vec![
        AgentEvent::TextDelta("hello".to_string()),
        AgentEvent::ToolStarted {
            name: "execute".to_string(),
            args: json!({ "command": "ls" }),
        },
        AgentEvent::ToolFinished {
            name: "execute".to_string(),
            output: ToolOutput::error("no such file"),
        },
        AgentEvent::Images {
            source: ImageSource::Tool("render_chart".to_string()),
            images: vec![crate::images::ImageRef {
                path: PathBuf::from("/tmp/chart.png"),
                mime: "image/png".to_string(),
                bytes: 4096,
            }],
        },
        AgentEvent::HookFired {
            event: "pre_tool_use".to_string(),
            command: "./check.sh".to_string(),
            outcome: crate::hooks::HookOutcome::Blocked("deploy is frozen".to_string()),
        },
        AgentEvent::TaskFinished {
            id: 3,
            command: "sleep 1".to_string(),
            status: crate::tools::tasks::TaskStatus::Done(0),
        },
        AgentEvent::PlanReady {
            plan: "1. do it".to_string(),
            gate,
        },
        AgentEvent::Interview {
            questions: vec![InterviewQuestion {
                question: "which database?".to_string(),
                options: vec!["sqlite".to_string(), "postgres".to_string()],
            }],
            gate: interview,
        },
        AgentEvent::Done {
            reason: DoneReason::MaxSteps,
        },
    ];

    for event in events {
        let written = serde_json::to_string(&event).expect("event serializes");
        let read_back: AgentEvent = serde_json::from_str(&written).expect("event deserializes");
        assert_eq!(
            serde_json::to_string(&read_back).expect("re-serializes"),
            written,
            "round trip changed the event: {written}"
        );
    }
}

/// Teeing the stream must not tee the answer. Two consumers can hold the same
/// request, which is the point of a clonable event, but the turn is waiting on
/// exactly one verdict, so the first to claim the gate is the one that gives
/// it and the second finds it spent.
#[test]
fn only_one_clone_of_a_plan_event_can_answer_it() {
    let (gate, mut verdict) = PlanGate::open();
    let event = AgentEvent::PlanReady {
        plan: "1. do it".to_string(),
        gate,
    };
    let teed = event.clone();

    let (AgentEvent::PlanReady { gate: first, .. }, AgentEvent::PlanReady { gate: second, .. }) =
        (event, teed)
    else {
        panic!("both copies are plan events");
    };
    assert!(
        first.answer(PlanVerdict::approve()),
        "the consumer that claims first answers"
    );
    assert!(
        !second.answer(PlanVerdict::reject("too big")),
        "the second consumer finds the gate already spent"
    );
    assert!(
        verdict.try_recv().expect("verdict delivered").approved,
        "the turn sees the first answer and only the first"
    );
}

/// Dropping a claimed gate is how a surface that goes away mid-review reports
/// "no verdict": the tool must come back to plan mode rather than park forever.
#[test]
fn a_dropped_claim_leaves_the_gate_unanswered() {
    let (gate, mut verdict) = PlanGate::open();
    drop(gate.claim().expect("the gate is open"));
    assert!(
        verdict.try_recv().is_err(),
        "no verdict was sent, and none can be now"
    );
    assert!(
        !gate.answer(PlanVerdict::approve()),
        "a claimed gate cannot be claimed again"
    );
}

/// Every shared handle the agent hands out has a surface holding it.
///
/// These accessors exist for exactly one reason: a turn takes the `Agent` out
/// of its slot, so anything a surface needs *during* a turn has to be a cloned
/// `Arc` it took beforehand. An accessor of this kind with no caller is not
/// harmless dead code — it is a feature that silently does not work at the one
/// moment it was built for, and it reads as finished because the accessor,
/// the registry and the tests around them are all present and green.
///
/// `task_registry` was exactly that. It shipped with the background-task
/// registry and was never called: `App` took `subagent_registry` and not this
/// one, so `/bashes` answered "unavailable while a turn is running" — during a
/// turn, which is the only time a background task exists to list.
///
/// `background_gate` was the same failure taken all the way. The gate, its
/// accessor and three doc comments describing a Ctrl-B that backgrounds the
/// running command were all present and green; `request` had no caller and no
/// tool ever read the gate, so the key did nothing and no document mentioned
/// it. It is gone rather than wired, because nobody asked for the feature.
///
/// Grepping is the honest instrument, as it is for the console-ownership test
/// in `trust.rs`: the failure is the *absence* of a call, and no runtime
/// assertion can observe a call that nobody makes.
#[test]
fn every_shared_registry_handle_is_held_by_a_surface() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");

    // Accessor -> the file that must take it before a turn starts.
    let wiring = [
        ("subagent_registry()", "app/runtime.rs"),
        ("task_registry()", "app/runtime.rs"),
    ];

    for (accessor, expected) in wiring {
        let mut callers: Vec<String> = Vec::new();
        for path in rust_sources(&root) {
            let rel = path
                .strip_prefix(&root)
                .expect("under src")
                .display()
                .to_string();
            // The definition itself lives in agent/mod.rs; a mention there is
            // the signature, not a call.
            if rel == "agent/mod.rs" || rel.ends_with("tests.rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("read a source file");
            if source.contains(accessor) {
                callers.push(rel);
            }
        }
        assert!(
            callers.iter().any(|c| c == expected),
            "`Agent::{accessor}` exists so a surface can use it while a turn holds the agent, \
             and {expected} does not take it. Found in: {callers:?}"
        );
    }
}

/// Exactly one function composes a session's tool registry.
///
/// The TUI used to have a second copy of `build_tool_registry` — same native +
/// scripted + MCP + spawner composition, a helper of its own for `evolve` and
/// `publish` — and every tool added to the shared builder had to be remembered
/// in it. `run_code` was not, so `code_mode = true` worked on `wizard -p`, ACP,
/// the gateway and the GUI and did nothing at all on the default surface, with
/// no refusal and no message, and `/reload` came back through the same copy.
///
/// A test that asserts a tool is registered can only ever check the composer it
/// knows about. This one asserts there is exactly one composer to know about,
/// which is the `contrib/find-unwired.py` defect class closed at the source
/// rather than one tool at a time. `SpawnSubagentTool::new` is the marker: a
/// registry that has the spawner on top is a session's registry.
#[test]
fn only_one_function_composes_a_sessions_tool_registry() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut composers: Vec<String> = Vec::new();
    for path in rust_sources(&root) {
        let rel = path
            .strip_prefix(&root)
            .expect("under src")
            .display()
            .to_string();
        if rel.ends_with("tests.rs") {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("read a source file");
        // Cut the inline test module off first: those build fixture
        // registries, not sessions.
        let production = source
            .split("\n#[cfg(test)]\nmod tests {")
            .next()
            .unwrap_or_default();
        if production.contains("SpawnSubagentTool::new") {
            composers.push(rel);
        }
    }
    composers.sort();
    assert_eq!(
        composers,
        vec!["agent/mod.rs".to_string()],
        "a surface is composing its own registry again; every tool added to \
         `build_tool_registry` now has to be remembered there too"
    );
}

/// Every `.rs` file under `dir`, recursively.
fn rust_sources(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(rust_sources(&path));
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            found.push(path);
        }
    }
    found
}

// -- code mode (`run_code`) -------------------------------------------------

/// Build the registry the way a real session does, for a given config.
async fn code_mode_registry(config: &Config) -> ToolRegistry {
    let tmp = TempDir::new();
    let client: Arc<dyn LlmProvider> = ScriptedProvider::new(Vec::new());
    let hooks = Arc::new(HookEngine::new(
        Vec::new(),
        tmp.0.clone(),
        "code-mode-test".to_string(),
    ));
    let manager = crate::mcp::McpManager::empty();
    let (registry, _model) = build_tool_registry(config, &client, &hooks, &manager)
        .await
        .expect("build the registry");
    registry
}

fn advertises(registry: &ToolRegistry, name: &str) -> bool {
    registry
        .specs()
        .iter()
        .any(|spec| spec.function.name == name)
}

/// Off by default, and off means absent from *both* rosters.
///
/// `specs()` is the sole input to the native `tools` array and to
/// `render_tool_protocol`, which is the JSON fallback's roster, so there is no
/// second place to remember — but that is a property worth asserting rather
/// than assuming, because a disabled code mode has to be byte-identical to a
/// build without it.
#[tokio::test]
async fn code_mode_is_off_by_default_and_absent_from_every_roster() {
    let config = Config::default();
    assert!(!config.code_mode, "the default must stay off");

    let registry = code_mode_registry(&config).await;
    assert!(
        registry
            .get(crate::tools::code::RUN_CODE_TOOL_NAME)
            .is_none(),
        "not registered"
    );
    assert!(!advertises(&registry, "run_code"), "not in the tools array");
    let protocol = prompts::render_tool_protocol(&registry.specs());
    assert!(
        !protocol.contains("run_code"),
        "and not in the JSON protocol roster either"
    );
}

/// The gate that is the whole requirement rather than belt and braces: a model
/// with no native tool calling never sees `run_code`, whatever the config says.
///
/// `render_tool_protocol` is deliberately full fidelity because it is the only
/// place such a model learns a tool exists, and the argument here is a
/// multi-line Lua program with quotes and backslashes inside a JSON string,
/// hand-emitted by a model `parse_json_tool_call` already assumes misformats
/// two-field calls. The failure is not "the program errors", it is "the JSON
/// does not parse and the turn stalls", on surfaces where nobody is watching.
#[tokio::test]
async fn code_mode_is_absent_on_the_json_protocol() {
    let tmp = TempDir::new();
    let config = Config {
        code_mode: true,
        ..Config::default()
    };
    // Asserted on the agent rather than on `build_tool_registry`, because the
    // agent is where the gate is and the registry is deliberately built with
    // the tool in it (see that function's doc: one gate, so a `/reload` on a
    // fallback model cannot leave a stale snapshot behind).
    let registry = code_mode_registry(&config).await;
    assert!(
        registry
            .get(crate::tools::code::RUN_CODE_TOOL_NAME)
            .is_some(),
        "the builder registers it whenever the config asks for it"
    );

    let session = Session::create(&tmp.0).expect("create session");
    let hooks = Arc::new(HookEngine::new(
        Vec::new(),
        tmp.0.clone(),
        session.id.clone(),
    ));
    let agent = Agent::new(
        ScriptedProvider::new(Vec::new()),
        registry,
        config,
        Vec::new(),
        tmp.0.clone(),
        session,
        false,
        hooks,
    )
    .expect("build agent");
    assert!(
        agent
            .dispatcher
            .registry()
            .get(crate::tools::code::RUN_CODE_TOOL_NAME)
            .is_none(),
        "a fallback model must not be offered a Lua program as an argument"
    );
    let protocol = prompts::render_tool_protocol(&agent.dispatcher.registry().specs());
    assert!(!protocol.contains("run_code"), "{protocol:.400}");
    assert!(
        !agent.history[0].text().contains("run_code"),
        "and the composed prompt must not name it either"
    );
}

/// A `/reload` performed while on a fallback model must not leave the stash
/// holding a snapshot of the tool set the reload replaced.
///
/// The sequence that used to do it: `/model <fallback>` takes `run_code` out of
/// the registry but keeps the stash, `/reload` hands over a registry the old gate
/// built *without* `run_code`, so the stash is not refreshed, and `/model
/// <native>` puts the pre-reload tool back — `wizard.tools()` listing a roster
/// that no longer exists and missing everything the reload added.
#[test]
fn a_reload_on_a_fallback_model_still_refreshes_the_code_mode_snapshot() {
    let tmp = TempDir::new();
    let config = Config {
        code_mode: true,
        ..Config::default()
    };
    let session = Session::create(&tmp.0).expect("create session");
    let hooks = Arc::new(HookEngine::new(
        Vec::new(),
        tmp.0.clone(),
        session.id.clone(),
    ));

    let stale = Arc::new(crate::tools::code::RunCodeTool::new(
        Arc::new(ToolRegistry::new()),
        Arc::clone(&hooks),
    ));
    let mut registry = ToolRegistry::new();
    registry.register(Arc::clone(&stale) as Arc<dyn crate::tools::Tool>);
    let mut agent = Agent::new(
        ScriptedProvider::new(Vec::new()),
        registry,
        config,
        Vec::new(),
        tmp.0.clone(),
        session,
        true,
        hooks.clone(),
    )
    .expect("build agent");
    agent.set_usage_log(Some(tmp.0.join("usage.jsonl")));

    // `/model` to something that cannot carry it.
    agent.set_model("a-small-local-model".to_string(), false);

    // `/reload`, which now always composes the tool.
    let fresh = Arc::new(crate::tools::code::RunCodeTool::new(
        Arc::new(ToolRegistry::new()),
        Arc::clone(&hooks),
    ));
    let mut reloaded = ToolRegistry::new();
    reloaded.register(Arc::clone(&fresh) as Arc<dyn crate::tools::Tool>);
    agent.set_registry(reloaded);
    let name = crate::tools::code::RUN_CODE_TOOL_NAME;
    assert!(
        agent.dispatcher.registry().get(name).is_none(),
        "still a fallback model, so still not advertised"
    );

    // ... and back.
    agent.set_model("a-model-with-tools".to_string(), true);
    let restored = agent
        .dispatcher
        .registry()
        .get(name)
        .expect("restored on the way back");
    assert!(
        Arc::ptr_eq(
            restored,
            &(Arc::clone(&fresh) as Arc<dyn crate::tools::Tool>)
        ),
        "the reload's tool, not the one it replaced"
    );
}

/// The `contrib/find-unwired.py` defect class, closed: written, documented,
/// green, and never actually registered.
#[tokio::test]
async fn code_mode_registers_when_enabled() {
    let config = Config {
        code_mode: true,
        ..Config::default()
    };
    let registry = code_mode_registry(&config).await;
    let tool = registry
        .get(crate::tools::code::RUN_CODE_TOOL_NAME)
        .expect("registered when the config asks for it and the model can carry it");
    assert_eq!(tool.access(), crate::tools::ToolAccess::Execute);
    assert_eq!(tool.kind(), crate::tools::ToolKind::Native);
    assert!(advertises(&registry, "run_code"));
}

/// A mid-session `/model` switch takes it away and gives it back.
///
/// Without this, switching to a small local model leaves a tool whose argument
/// is a multi-line Lua program advertised on the JSON fallback protocol, which
/// does not fail loudly — it stalls the turn.
#[test]
fn switching_to_a_fallback_model_removes_run_code_and_switching_back_restores_it() {
    let tmp = TempDir::new();
    let config = Config {
        code_mode: true,
        ..Config::default()
    };
    let provider = ScriptedProvider::new(Vec::new());
    let session = Session::create(&tmp.0).expect("create session");
    let hooks = Arc::new(HookEngine::new(
        Vec::new(),
        tmp.0.clone(),
        session.id.clone(),
    ));
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(crate::tools::code::RunCodeTool::new(
        Arc::new(ToolRegistry::new()),
        Arc::clone(&hooks),
    )));
    let mut agent = Agent::new(
        provider,
        registry,
        config,
        Vec::new(),
        tmp.0.clone(),
        session,
        true,
        hooks,
    )
    .expect("build agent");
    agent.set_usage_log(Some(tmp.0.join("usage.jsonl")));

    let name = crate::tools::code::RUN_CODE_TOOL_NAME;
    assert!(agent.dispatcher.registry().get(name).is_some());

    agent.set_model("a-small-local-model".to_string(), false);
    assert!(
        agent.dispatcher.registry().get(name).is_none(),
        "a fallback model must not keep it"
    );
    let prompt = agent.history[0].text();
    assert!(
        !prompt.contains("run_code"),
        "and the recomposed JSON protocol section must not name it: {prompt:.400}"
    );

    agent.set_model("a-model-with-tools".to_string(), true);
    assert!(
        agent.dispatcher.registry().get(name).is_some(),
        "and switching back restores it, from the stash rather than a rebuild"
    );
}
