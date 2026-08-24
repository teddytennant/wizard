//! The host bridge, driven from a real Lua plugin.
//!
//! Every test below loads the same fixture — a plugin directory with a
//! `manifest.toml` and a `plugin.lua`, exactly as one installed under
//! `~/.wizard/plugins` — into a kernel whose host is [`WizardHost`], and calls
//! its tools the way the agent's dispatcher would. Nothing here asserts
//! against a mock of the bridge: the point of the change these tests cover is
//! that `wizard.http` reaches a socket, `wizard.model` reaches a provider and
//! spends on a tracker, and `wizard.process` reaches a child process, so an
//! assertion against a recording host would prove none of it.

use std::sync::Mutex;

use serde_json::json;

use super::*;
use crate::agent::subagent::SPAWN_SUBAGENT_TOOL_NAME;
use crate::kernel::testing::TempDir;
use crate::kernel::{Kernel, KernelOptions, PluginSource};
use crate::llm::{CacheTokens, ChatChunk, ChatStream};
use crate::tools::registry::ToolRegistry;
use crate::usage::UsageTracker;

/// The one fixture plugin, exercising every gated namespace.
///
/// One plugin rather than six, because the interesting question is not "does
/// `wizard.http` work in isolation" but "does a plugin that declared the whole
/// table get the whole table" — and because the absence rule is only testable
/// against the same script loaded under a *smaller* manifest, which is what
/// the second load below does.
const FIXTURE: &str = r#"
return {
  apply = function(ctx)
    ctx:tool { name = "p_fetch", execute = function(args)
      return wizard.http.get(args.url)
    end }
    ctx:tool { name = "p_post", execute = function(args)
      return wizard.http.post(args.url, args.body)
    end }
    ctx:tool { name = "p_ask", execute = function(args)
      return wizard.model.complete(args.prompt)
    end }
    ctx:tool { name = "p_say", execute = function(args)
      wizard.ui.notify(args.text)
      return "said"
    end }
    ctx:tool { name = "p_run", execute = function(args)
      return wizard.process.run(args.command)
    end }
    ctx:tool { name = "p_delegate", execute = function(args)
      return wizard.agent.spawn(args.task)
    end }
    ctx:tool { name = "p_tables", execute = function()
      return table.concat({
        tostring(wizard.http ~= nil), tostring(wizard.model ~= nil),
        tostring(wizard.ui ~= nil), tostring(wizard.agent ~= nil),
        tostring(wizard.process ~= nil), tostring(wizard.fs ~= nil),
      }, ",")
    end }
  end,
}
"#;

const ALL_CAPS: &str = "name = \"probe\"\nversion = \"1.0.0\"\n\
     capabilities = [\"network\", \"model\", \"ui\", \"agent\", \"process\", \"filesystem\"]\n";

/// A kernel carrying the real bridge, with `manifest` + [`FIXTURE`] loaded.
async fn fixture_with(dir: &TempDir, name: &str, manifest: &str) -> (Kernel, Arc<WizardHost>) {
    let host = Arc::new(WizardHost::new(&dir.path));
    let kernel = Kernel::new(KernelOptions {
        project_root: dir.path.clone(),
        plugin_root: dir.path.join("plugins"),
        host: Arc::clone(&host) as Arc<dyn HostBridge>,
        ..Default::default()
    });
    let plugin = dir.write_plugin(name, manifest, FIXTURE);
    kernel
        .load_lua(&plugin, PluginSource::FirstParty)
        .await
        .expect("the fixture loads");
    (kernel, host)
}

async fn fixture(dir: &TempDir) -> (Kernel, Arc<WizardHost>) {
    fixture_with(dir, "probe", ALL_CAPS).await
}

/// Call one of the fixture's tools the way the dispatcher would.
async fn call(kernel: &Kernel, tool: &str, args: serde_json::Value) -> Result<String, String> {
    let tool = kernel.tool(tool).expect("the fixture registered it");
    let ctx = ToolContext::new(kernel.project_root());
    match tool.execute(args, &ctx).await {
        Ok(out) if out.is_error => Err(out.content),
        Ok(out) => Ok(out.content),
        // Through `anyhow` for the `{:#}` chain: a `ToolError`'s own `Display`
        // is just "tool 'x' failed", and everything these tests assert on is
        // in the source underneath it.
        Err(err) => Err(format!("{:#}", anyhow::Error::new(err))),
    }
}

/// Provider that answers with `text` and reports the token counts a real one
/// would, so the metering assertions have something to read.
#[derive(Debug)]
struct OneAnswer {
    text: String,
    prompts: Mutex<Vec<crate::llm::ChatRequest>>,
}

impl OneAnswer {
    fn arc(text: &str) -> Arc<Self> {
        Arc::new(Self {
            text: text.to_string(),
            prompts: Mutex::new(Vec::new()),
        })
    }

    fn asked(&self) -> Vec<crate::llm::ChatRequest> {
        self.prompts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait::async_trait]
impl LlmProvider for OneAnswer {
    async fn health(&self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn supports_native_tools(&self, _model: &str) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn list_models(&self) -> anyhow::Result<Vec<String>> {
        Ok(Vec::new())
    }

    async fn chat_stream(&self, request: crate::llm::ChatRequest) -> anyhow::Result<ChatStream> {
        self.prompts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request);
        let chunk = ChatChunk {
            message: Some(ChatMessage::assistant(&self.text)),
            images: Vec::new(),
            thinking: false,
            done: true,
            done_reason: None,
            eval_count: Some(7),
            prompt_eval_count: Some(11),
            cache: CacheTokens { read: 3, write: 2 },
        };
        Ok(futures_util::StreamExt::boxed(futures_util::stream::iter(
            vec![Ok(chunk)],
        )))
    }

    fn label(&self) -> String {
        "one-answer".to_string()
    }
}

/// A binding around `ctx`, with a provider that answers `answer`.
fn binding(ctx: ToolContext, answer: &str) -> (Binding, Arc<OneAnswer>) {
    let client = OneAnswer::arc(answer);
    (
        Binding {
            ctx,
            client: Arc::clone(&client) as Arc<dyn LlmProvider>,
            model: "test-model".to_string(),
            spawn: None,
        },
        client,
    )
}

/// `[web]` with loopback reachable, which is what makes a test server
/// addressable at all.
fn local_web(dir: &TempDir) -> ToolContext {
    ToolContext::new(&dir.path).with_web(crate::config::WebConfig {
        allow_local: true,
        ..Default::default()
    })
}

// ---------------------------------------------------------------------------
// The capability table, as absence
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_plugin_that_declared_everything_gets_everything() {
    let dir = TempDir::new("host-tables");
    let (kernel, _host) = fixture(&dir).await;
    assert_eq!(
        call(&kernel, "p_tables", json!({})).await.unwrap(),
        "true,true,true,true,true,true"
    );
}

#[tokio::test]
async fn a_capability_a_plugin_did_not_declare_is_absent_from_the_wired_host_too() {
    // Absence is pinned in `lua::host` against the recording bridge already.
    // Repeated here against the real one because "absent, not refusing" is a
    // claim about the *wired* build, and wiring a namespace up is exactly the
    // change that could turn an absent table into a present one that errors.
    let dir = TempDir::new("host-absence");
    let (kernel, _host) = fixture_with(
        &dir,
        "bare",
        "name = \"bare\"\nversion = \"1.0.0\"\ncapabilities = [\"network\"]\n",
    )
    .await;
    // `fs` is the exception and is meant to be: without `filesystem` it is
    // confined to the project root, not taken away.
    assert_eq!(
        call(&kernel, "p_tables", json!({})).await.unwrap(),
        "true,false,false,false,false,true"
    );
}

// ---------------------------------------------------------------------------
// wizard.http
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_plugin_fetches_a_url_through_the_web_tools_client() {
    let dir = TempDir::new("host-http");
    let (kernel, host) = fixture(&dir).await;
    let root = crate::llm::test_support::one_shot_http_server(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 15\r\n\r\n{\"ok\":\"plugin\"}",
    )
    .await;
    host.bind(binding(local_web(&dir), "").0);

    let body = call(&kernel, "p_fetch", json!({ "url": format!("{root}/x") }))
        .await
        .unwrap();
    assert_eq!(body, "{\"ok\":\"plugin\"}");
}

#[tokio::test]
async fn a_plugin_fetch_obeys_the_web_configs_ssrf_guard() {
    let dir = TempDir::new("host-ssrf");
    let (kernel, host) = fixture(&dir).await;
    // The default `[web]`, i.e. `allow_local = false`. The guard is the web
    // tool's, so a plugin cannot reach the loopback services the web tool
    // cannot reach either.
    host.bind(binding(ToolContext::new(&dir.path), "").0);
    let err = call(&kernel, "p_fetch", json!({ "url": "http://127.0.0.1:1/x" }))
        .await
        .unwrap_err();
    assert!(err.contains("local"), "{err}");
}

#[tokio::test]
async fn a_plugin_fetch_caps_the_body_at_fetch_max_bytes() {
    let dir = TempDir::new("host-cap");
    let (kernel, host) = fixture(&dir).await;
    let root = crate::llm::test_support::one_shot_http_server(
        "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 40\r\n\r\n\
         0123456789012345678901234567890123456789",
    )
    .await;
    let ctx = ToolContext::new(&dir.path).with_web(crate::config::WebConfig {
        allow_local: true,
        fetch_max_bytes: 8,
        ..Default::default()
    });
    host.bind(binding(ctx, "").0);

    let body = call(&kernel, "p_fetch", json!({ "url": root }))
        .await
        .unwrap();
    assert!(body.starts_with("01234567"), "{body}");
    assert!(body.contains("capped at 8 bytes"), "{body}");
}

#[tokio::test]
async fn a_plugin_post_is_not_redirected_with_its_body() {
    let dir = TempDir::new("host-post");
    let (kernel, host) = fixture(&dir).await;
    let root = crate::llm::test_support::one_shot_http_server(
        "HTTP/1.1 302 Found\r\nlocation: http://example.invalid/\r\ncontent-length: 0\r\n\r\n",
    )
    .await;
    host.bind(binding(local_web(&dir), "").0);

    let err = call(
        &kernel,
        "p_post",
        json!({ "url": root, "body": "secret=1" }),
    )
    .await
    .unwrap_err();
    assert!(err.contains("redirected"), "{err}");
    assert!(err.contains("did not name"), "{err}");
}

#[tokio::test]
async fn a_plugin_fetch_stops_when_the_turn_is_cancelled() {
    let dir = TempDir::new("host-http-cancel");
    let (kernel, host) = fixture(&dir).await;
    let cancel = crate::agent::CancelHandle::default();
    cancel.cancel();
    host.bind(binding(local_web(&dir).with_cancel(cancel), "").0);

    // Nothing is listening on port 1, so a fetch that ignored the handle would
    // still fail — with a connection error. The message is what tells the two
    // outcomes apart.
    let err = call(&kernel, "p_fetch", json!({ "url": "http://127.0.0.1:1/" }))
        .await
        .unwrap_err();
    assert!(err.contains("interrupted"), "{err}");
}

// ---------------------------------------------------------------------------
// wizard.model
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_plugin_model_call_answers_and_is_billed_to_the_agents_tracker() {
    let dir = TempDir::new("host-model");
    let (kernel, host) = fixture(&dir).await;
    let usage = Arc::new(UsageTracker::new());
    let ctx = ToolContext::new(&dir.path).with_usage(Arc::clone(&usage));
    let (bound, client) = binding(ctx, "forty-two");
    host.bind(bound);

    let answer = call(&kernel, "p_ask", json!({ "prompt": "how many?" }))
        .await
        .unwrap();
    assert_eq!(answer, "forty-two");

    // The whole argument for gating `model` behind a capability is that it
    // spends the user's money, so the spend has to land where `/cost` reads —
    // which is exactly these four counters.
    assert_eq!(usage.session_totals(), (11, 7));
    assert_eq!(usage.session_cache_totals(), (3, 2));
    // And not into `last_prompt`, which drives compaction: a plugin's prompt
    // is not this turn's prompt, the same reason a subagent's is not.
    assert_eq!(usage.last_prompt_tokens(), None);

    let asked = client.asked();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].model, "test-model");
    assert!(
        asked[0].tools.is_empty(),
        "a plugin's question carries no tools; `wizard.agent.spawn` is the call that does"
    );
}

#[tokio::test]
async fn a_plugin_model_call_is_refused_when_no_agent_is_bound() {
    let dir = TempDir::new("host-model-unbound");
    let (kernel, _host) = fixture(&dir).await;
    let err = call(&kernel, "p_ask", json!({ "prompt": "hi" }))
        .await
        .unwrap_err();
    assert!(err.contains("no agent is attached"), "{err}");
}

#[tokio::test]
async fn a_plugin_model_call_stops_when_the_turn_is_cancelled() {
    let dir = TempDir::new("host-model-cancel");
    let (kernel, host) = fixture(&dir).await;
    let cancel = crate::agent::CancelHandle::default();
    cancel.cancel();
    let (bound, client) = binding(
        ToolContext::new(&dir.path).with_cancel(cancel),
        "never read",
    );
    host.bind(bound);

    let err = call(&kernel, "p_ask", json!({ "prompt": "hi" }))
        .await
        .unwrap_err();
    assert!(err.contains("interrupted"), "{err}");
    assert!(
        client.asked().is_empty(),
        "a cancelled call must not reach the provider at all"
    );
}

// ---------------------------------------------------------------------------
// wizard.ui
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_plugin_notice_reaches_the_turns_transcript_with_its_author() {
    let dir = TempDir::new("host-ui");
    let (kernel, host) = fixture(&dir).await;
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    host.bind(binding(ToolContext::new(&dir.path).with_events(tx), "").0);

    assert_eq!(
        call(&kernel, "p_say", json!({ "text": "hello" }))
            .await
            .unwrap(),
        "said"
    );
    match rx.try_recv().expect("a notice was emitted") {
        AgentEvent::Notice(line) => assert_eq!(line, "[probe] hello"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_plugin_notice_degrades_to_the_log_with_no_surface_attached() {
    let dir = TempDir::new("host-ui-unbound");
    let (kernel, _host) = fixture(&dir).await;
    // Not an error. A notice's failure mode is nobody hearing it, and the log
    // is somewhere it can be heard.
    assert_eq!(
        call(&kernel, "p_say", json!({ "text": "hi" }))
            .await
            .unwrap(),
        "said"
    );
}

// ---------------------------------------------------------------------------
// wizard.process
// ---------------------------------------------------------------------------

#[tokio::test]
#[cfg(unix)]
async fn a_plugin_command_runs_in_the_project_root_with_no_agent_bound() {
    let dir = TempDir::new("host-run");
    let (kernel, _host) = fixture(&dir).await;
    std::fs::write(dir.path.join("marker.txt"), "here").expect("write");

    // Deliberately unbound: `process` needs no agent, and the directory it
    // falls back to is the kernel's project root — the same root a sandboxed
    // plugin's file helpers are confined to.
    let out = call(&kernel, "p_run", json!({ "command": "cat marker.txt" }))
        .await
        .unwrap();
    assert_eq!(out.trim(), "here");
}

#[tokio::test]
#[cfg(unix)]
async fn a_failed_plugin_command_is_an_error_carrying_its_output() {
    let dir = TempDir::new("host-run-fail");
    let (kernel, _host) = fixture(&dir).await;
    let err = call(
        &kernel,
        "p_run",
        json!({ "command": "echo nope >&2; exit 3" }),
    )
    .await
    .unwrap_err();
    assert!(err.contains("exited 3"), "{err}");
    assert!(err.contains("nope"), "{err}");
}

#[tokio::test]
#[cfg(unix)]
async fn a_plugin_command_stops_when_the_turn_is_cancelled() {
    let dir = TempDir::new("host-run-cancel");
    let (kernel, host) = fixture(&dir).await;
    let cancel = crate::agent::CancelHandle::default();
    host.bind(binding(ToolContext::new(&dir.path).with_cancel(cancel.clone()), "").0);

    let waiting =
        tokio::spawn(async move { call(&kernel, "p_run", json!({ "command": "sleep 60" })).await });
    // Long enough for the child to exist, far short of the `[shell]` budget:
    // a run that ignored the handle would fail on the budget instead, in
    // thirty seconds rather than in one.
    tokio::time::sleep(Duration::from_millis(250)).await;
    cancel.cancel();

    let err = waiting.await.expect("the task joined").unwrap_err();
    assert!(err.contains("interrupted"), "{err}");
}

// ---------------------------------------------------------------------------
// wizard.agent
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_plugin_subagent_runs_through_the_spawn_tool() {
    let dir = TempDir::new("host-agent");
    let (kernel, host) = fixture(&dir).await;
    let client = OneAnswer::arc("the worker's report");
    let hooks = Arc::new(crate::hooks::HookEngine::new(
        Vec::new(),
        dir.path.clone(),
        "host-test".to_string(),
    ));
    let spawn = Arc::new(crate::agent::subagent::SpawnSubagentTool::new(
        crate::agent::subagent::builtin_configs(),
        Arc::clone(&client) as Arc<dyn LlmProvider>,
        Arc::new(ToolRegistry::new()),
        hooks,
    ));
    assert_eq!(spawn.name(), SPAWN_SUBAGENT_TOOL_NAME);
    host.bind(Binding {
        ctx: ToolContext::new(&dir.path),
        client: Arc::clone(&client) as Arc<dyn LlmProvider>,
        model: "test-model".to_string(),
        spawn: Some(spawn as Arc<dyn Tool>),
    });

    let out = call(&kernel, "p_delegate", json!({ "task": "count the files" }))
        .await
        .unwrap();
    assert!(out.contains("the worker's report"), "{out}");

    // The plugin names itself in the brief, so a user reading a subagent pane
    // can tell who asked for the run.
    let asked = client.asked();
    let brief = format!("{:?}", asked.last().expect("the sub-run asked the model"));
    assert!(brief.contains("'probe' plugin"), "{brief}");
}

#[tokio::test]
async fn a_plugin_subagent_is_refused_when_no_agent_is_bound() {
    let dir = TempDir::new("host-agent-unbound");
    let (kernel, _host) = fixture(&dir).await;
    let err = call(&kernel, "p_delegate", json!({ "task": "x" }))
        .await
        .unwrap_err();
    assert!(err.contains("no agent is attached"), "{err}");
}
