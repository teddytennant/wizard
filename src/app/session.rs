//! Agent wiring and provider startup: building the tool registry and
//! [`Agent`], resolving a working LLM client, and the background rebuild
//! tasks (`/model`, crash recovery).

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::{Mutex, mpsc};

use crate::agent::{Agent, session::Session, subagent};
use crate::config::{Config, ProviderConfig, ProviderKind};
use crate::event::Event;
use crate::hooks::HookEngine;
use crate::llm::provider::LlmProvider;
use crate::mcp::McpManager;
use crate::server;
use crate::skills::Skill;
use crate::tools::CommandDispatch;
use crate::tools::registry::ToolRegistry;

use super::{AgentRebuild, App};

/// Load skills from the canonical roots (repo checkout, bundled beside the
/// binary, `~/.wizard/skills/`; later roots shadow earlier ones).
pub(super) fn load_skill_roots() -> Vec<Skill> {
    let roots = crate::skills::default_roots();
    match crate::skills::load_skills(&roots) {
        Ok(skills) => skills,
        Err(err) => {
            tracing::warn!("loading skills: {err:#}");
            Vec::new()
        }
    }
}

/// The TUI's registry, which is [`crate::agent::build_tool_registry`] and
/// nothing else.
///
/// It used to be a second copy of that function — same native/scripted/MCP
/// composition, same subagent spawner — with `evolve` and `publish` bolted on
/// afterwards by a helper of its own. Every tool added to the shared builder had
/// to be remembered here too, and `run_code` was not: `code_mode = true` in
/// `~/.wizard/config.toml` gave the tool to `wizard -p`, ACP, the gateway and the
/// GUI, and silently gave the interactive TUI nothing — not refused, not
/// explained, and `/reload` did not fix it, because `/reload` came back through
/// the same copy. The duplication was the defect; deleting it is the fix.
pub(super) async fn build_registry(
    config: &Config,
    manager: &McpManager,
    client: &Arc<dyn LlmProvider>,
    hooks: &Arc<HookEngine>,
) -> Result<(ToolRegistry, subagent::SharedActiveModel)> {
    crate::agent::build_tool_registry(config, client, hooks, manager).await
}

/// Which session a freshly built [`Agent`] attaches to.
#[derive(Debug, Clone)]
pub(super) enum SessionTarget {
    /// Start a brand-new session file.
    Fresh,
    /// Reopen the most recent session (`--resume`).
    Latest,
    /// Reopen a specific session by id (`/resume`, and crash/interrupt
    /// recovery of the active session so it survives a prior `/resume`).
    Id(String),
}

pub(super) async fn build_agent(
    client: &Arc<dyn LlmProvider>,
    config: &Config,
    skills: &[Skill],
    project_root: &Path,
    manager: &McpManager,
    resume: SessionTarget,
) -> Result<Agent> {
    // Session first: the hook engine carries its id in every payload.
    let sessions_dir = Config::sessions_dir()?;
    let open_latest_or_fresh = || match Session::open_latest(&sessions_dir)? {
        Some(session) => Ok(session),
        None => Session::create(&sessions_dir),
    };
    let session = match resume {
        SessionTarget::Fresh => Session::create(&sessions_dir)?,
        SessionTarget::Latest => open_latest_or_fresh()?,
        SessionTarget::Id(id) => match Session::open_by_id(&sessions_dir, &id)? {
            Some(session) => session,
            // The id vanished (deleted, or empty after a fallback) — degrade
            // to the latest session rather than silently starting blank.
            None => open_latest_or_fresh()?,
        },
    };
    let hooks = Arc::new(HookEngine::new(
        crate::hooks::load(project_root),
        project_root.to_path_buf(),
        session.id.clone(),
    ));
    let (registry, subagent_model) = build_registry(config, manager, client, &hooks).await?;
    let model = config.active().model;
    let native_tools = crate::llm::provider::probe_native_tools(client.as_ref(), &model).await;
    let mut agent = Agent::new(
        Arc::clone(client),
        registry,
        config.clone(),
        skills.to_vec(),
        project_root.to_path_buf(),
        session,
        native_tools,
        hooks,
    )?;
    // The TUI has a home for every command the agent may queue, so it dispatches
    // all of them (the GUI runs the subset its executor implements; headless and
    // gateway runs, none).
    agent.set_command_dispatch(CommandDispatch::All);
    // The TUI is the one surface with a person at a keyboard, so it is the one
    // surface where a shell command that asks a question can be answered: its
    // composer claims the console gate and relays what is typed. Everywhere
    // else leaves this at `None` and keeps `/dev/null` on fd 0.
    agent.set_console_access(crate::tools::ConsoleAccess::Interactive);
    agent.bind_subagent_model(subagent_model);
    Ok(agent)
}

/// Rebuild the agent from the current session on a background task (crash and
/// forced-interrupt recovery), so the TUI stays responsive. The outcome lands
/// via [`Event::AgentRebuilt`]: `success_notice` on success, a "/quit and
/// relaunch" notice on failure.
pub(super) fn spawn_session_rebuild(
    app: &mut App,
    client: &Arc<dyn LlmProvider>,
    skills: &[Skill],
    project_root: &Path,
    manager: &Arc<Mutex<McpManager>>,
    notify: mpsc::Sender<Event>,
    success_notice: &str,
) {
    app.rebuilding = Some("restarting agent".to_string());
    let client = Arc::clone(client);
    let config = app.config.clone();
    let skills = skills.to_vec();
    let project_root = project_root.to_path_buf();
    let manager = Arc::clone(manager);
    let session = SessionTarget::Id(app.session_id.clone());
    let success_notice = success_notice.to_string();
    // `App::rebuilding` is cleared only by the `AgentRebuilt` below, and while
    // it is set the main loop refuses to start any turn. A panic in here
    // (session decode, a provider client's model probe) therefore did not just
    // lose the rebuild — it made every later message queue silently, forever.
    // See [`crate::app::recover::spawn_answering`].
    super::recover::spawn_answering(
        notify,
        Event::AgentRebuilt(Box::new(AgentRebuild {
            agent: None,
            model: None,
            notice: "the agent rebuild crashed".to_string(),
        })),
        async move {
            // Bounded, because the `McpManager` lock below is shared with the
            // startup connect: a stdio server that never answers its handshake
            // would otherwise park this rebuild forever, and a rebuild that
            // never lands is a session that never runs another turn.
            let built = super::recover::within(
                "restarting the agent",
                super::recover::AGENT_REBUILD_DEADLINE,
                async {
                    let manager = manager.lock().await;
                    build_agent(&client, &config, &skills, &project_root, &manager, session).await
                },
            )
            .await;
            let rebuild = match built {
                Ok(Ok(agent)) => AgentRebuild {
                    agent: Some(agent),
                    model: None,
                    notice: success_notice,
                },
                Ok(Err(err)) => AgentRebuild {
                    agent: None,
                    model: None,
                    notice: format!("could not restart the agent: {err:#}"),
                },
                Err(timed_out) => AgentRebuild {
                    agent: None,
                    model: None,
                    notice: timed_out,
                },
            };
            Some(Event::AgentRebuilt(Box::new(rebuild)))
        },
    );
}

/// Re-arm `/ultra` on a freshly built agent, and a no-op when ultra is off.
///
/// [`build_agent`] hands back an agent with ultra unset, so every rebuild — a
/// `/model` switch, a provider switch, a `/resume` — silently drops the
/// mixture-of-agents pre-phase while [`App::ultra`] keeps the badge lit and the
/// user keeps paying attention to a fan-out that is no longer happening. Call
/// this at every site that installs a rebuilt agent.
///
/// The engine holds no client: the agent supplies the live one at run time. So
/// the same handle re-arms unchanged across a model switch, and the candidates
/// simply follow whatever model is now active — which is the whole point of
/// ultra being agent-level rather than model-level.
pub(super) fn restore_ultra(app: &App, agent: &mut Agent) {
    agent.set_ultra(app.ultra.clone());
}

/// True for backends that run on this machine (no API key, no cloud).
pub(super) fn is_local_kind(kind: ProviderKind) -> bool {
    matches!(kind, ProviderKind::LlamaCpp | ProviderKind::Ollama)
}

/// Build `provider`'s client and prove it usable: for local llama.cpp this
/// spawns `llama-server` when possible (the terminal is still in normal mode
/// at startup, so spawn/load progress shows on a plain-terminal spinner),
/// then runs the provider's health probe.
pub(super) async fn try_provider(provider: &ProviderConfig) -> Result<Arc<dyn LlmProvider>> {
    let client = provider
        .build()
        .with_context(|| format!("building provider '{}'", provider.name))?;
    if provider.kind == ProviderKind::LlamaCpp {
        let wait = crate::progress::ServerSpinner::start();
        let outcome = server::ensure_running(provider, &wait).await;
        wait.finish(outcome.is_ok());
        outcome?;
    }
    // Ollama gets the same first-run hand for the model itself: a configured
    // tag that is not pulled yet (onboarding's BYOM pick, a hand-written
    // config) is pulled now with visible progress. Loopback hosts only —
    // Wizard never downloads models onto a remote server.
    if provider.kind == ProviderKind::Ollama && server::local_port(&provider.base_url).is_some() {
        let wait =
            crate::progress::ServerSpinner::start_with("Checking the local model…", "model ready");
        let outcome = crate::llm::ollama::OllamaClient::new(provider.base_url.clone())
            .ensure_model(&provider.model, &wait)
            .await;
        wait.finish(outcome.is_ok());
        outcome?;
    }
    client
        .health()
        .await
        .with_context(|| format!("LLM health check failed for {}", client.label()))?;
    Ok(client)
}

/// Cloud providers synthesized from standard API-key env vars when the local
/// backend is unavailable and nothing usable is configured:
/// `(key env var, kind, base URL, model, provider name)`.
const BYOP_ENV_FALLBACKS: &[(&str, ProviderKind, &str, &str, &str)] = &[
    (
        "ANTHROPIC_API_KEY",
        ProviderKind::Anthropic,
        "https://api.anthropic.com",
        "claude-fable-5",
        "anthropic",
    ),
    (
        "OPENAI_API_KEY",
        ProviderKind::Openai,
        "https://api.openai.com/v1",
        "gpt-4o",
        "openai",
    ),
    (
        "XAI_API_KEY",
        ProviderKind::Xai,
        "https://api.x.ai/v1",
        "grok-4.6",
        "xai",
    ),
    (
        "OPENROUTER_API_KEY",
        ProviderKind::OpenRouter,
        "https://openrouter.ai/api/v1",
        "openrouter/auto",
        "openrouter",
    ),
];

/// Resolve a working LLM client at startup. The active provider is tried
/// first. A failing *local* backend (llama.cpp not installed, server not
/// running, no model file, …) is not fatal: Wizard falls back to
/// bring-your-own-provider — any configured cloud provider, then one
/// synthesized from a standard API-key env var, then (interactively) the
/// onboarding wizard. The chosen fallback becomes the active provider in the
/// in-memory config so the session's picker and status bar reflect it; only
/// onboarding persists anything to disk.
pub(super) async fn startup_client(config: &mut Config) -> Result<Arc<dyn LlmProvider>> {
    let active = config.active();
    // Cloud provider: build the client (cheap — it just reads cached
    // credentials) and return immediately. The health probe is a network
    // round-trip that would block the first paint, so `run_tui` runs it in the
    // background and surfaces a failure as a notice. A *build* error is a config
    // error (e.g. malformed base URL), so it stays fatal. The local fallback
    // chain below only matters when a local backend is the active provider.
    if !is_local_kind(active.kind) {
        return active
            .build()
            .with_context(|| format!("building provider '{}'", active.name));
    }
    let local_err = match try_provider(&active).await {
        Ok(client) => return Ok(client),
        Err(err) => err,
    };
    println!("local model unavailable: {local_err:#}");

    // Any other configured cloud provider.
    for provider in config.providers.clone() {
        if is_local_kind(provider.kind) || provider.name == active.name {
            continue;
        }
        match try_provider(&provider).await {
            Ok(client) => {
                println!(
                    "falling back to provider '{}' ({})",
                    provider.name, provider.model
                );
                config.active_provider = Some(provider.name);
                return Ok(client);
            }
            Err(err) => println!("provider '{}' is also unavailable: {err:#}", provider.name),
        }
    }

    // A provider synthesized from a standard API-key env var.
    for &(key_env, kind, base_url, model, name) in BYOP_ENV_FALLBACKS {
        if !std::env::var(key_env).is_ok_and(|v| !v.trim().is_empty()) {
            continue;
        }
        let provider = ProviderConfig {
            name: name.to_string(),
            kind,
            base_url: base_url.to_string(),
            model: model.to_string(),
            api_key_env: Some(key_env.to_string()),
            gguf_path: None,
            usd_per_mtok_in: None,
            usd_per_mtok_out: None,
        };
        match try_provider(&provider).await {
            Ok(client) => {
                println!("falling back to {model} via ${key_env}");
                // Replace any same-named (failed) entry so active() resolves
                // to this one.
                config.providers.retain(|p| p.name != provider.name);
                config.active_provider = Some(provider.name.clone());
                config.providers.push(provider);
                return Ok(client);
            }
            Err(err) => println!("{name} via ${key_env} is also unavailable: {err:#}"),
        }
    }

    // Nothing usable: let the user bring their own provider interactively.
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        println!("no working provider — opening setup so you can pick one (Esc to cancel).");
        if let Some(new_config) = crate::onboarding::run().await? {
            let active = new_config.active();
            let client = try_provider(&active).await?;
            *config = new_config;
            return Ok(client);
        }
    }

    Err(local_err.context(
        "the local model is unavailable and no fallback provider is configured — \
         run `wizard --onboard` to set one up",
    ))
}

/// Background half of `/model <tag>`: validate the tag against the
/// installed models, probe native tool support, then either retag the live
/// agent (context preserved) or build a fresh one.
pub(super) async fn switch_model_task(
    agent: Option<Agent>,
    tag: String,
    client: &Arc<dyn LlmProvider>,
    mut config: Config,
    skills: Vec<Skill>,
    project_root: PathBuf,
    manager: Arc<Mutex<McpManager>>,
) -> AgentRebuild {
    if let Ok(models) = client.list_models().await {
        let known = models
            .iter()
            .any(|m| *m == tag || m.split(':').next() == Some(tag.as_str()));
        if !known {
            // Hand the untouched agent straight back.
            return AgentRebuild {
                agent,
                model: None,
                notice: format!("model '{tag}' is not installed (try `ollama pull {tag}`)"),
            };
        }
    }
    let native_tools = crate::llm::provider::probe_native_tools(client.as_ref(), &tag).await;
    match agent {
        Some(mut agent) => {
            agent.set_model(tag.clone(), native_tools);
            AgentRebuild {
                agent: Some(agent),
                model: Some(tag.clone()),
                notice: format!("switched to model {tag} (context preserved)"),
            }
        }
        None => {
            config.model = tag.clone();
            let manager = manager.lock().await;
            match build_agent(
                client,
                &config,
                &skills,
                &project_root,
                &manager,
                SessionTarget::Fresh,
            )
            .await
            {
                Ok(agent) => AgentRebuild {
                    agent: Some(agent),
                    model: Some(tag.clone()),
                    notice: format!("switched to model {tag}"),
                },
                Err(err) => AgentRebuild {
                    agent: None,
                    model: None,
                    notice: format!("failed to switch model: {err:#}"),
                },
            }
        }
    }
}
