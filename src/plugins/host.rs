//! The real [`HostBridge`]: what `wizard.http`, `wizard.model`, `wizard.ui`,
//! `wizard.agent` and `wizard.process` actually do.
//!
//! `docs/plugins.md` left this as the open piece — "a Lua plugin that calls
//! `wizard.http` or `wizard.model` gets an error naming the reason". This
//! module is the answer, and its whole design is a refusal to write anything
//! twice. Every namespace below resolves to code that already exists and is
//! already tested:
//!
//! | Namespace | Reached through |
//! | --- | --- |
//! | `wizard.http` | [`crate::tools::web`]'s client, SSRF guard and body cap |
//! | `wizard.process` | [`crate::tools::shell::run_command_cancellable`] |
//! | `wizard.model` | the agent's live [`LlmProvider`], billed to its tracker |
//! | `wizard.ui` | the turn's [`AgentEvent`] channel |
//! | `wizard.agent` | the registered `spawn_subagent` tool |
//!
//! The two that could have been written fresh and were not are worth naming.
//! A second HTTP client is a second place to forget that reqwest's redirect
//! policy is synchronous and therefore cannot re-resolve a hop, which is the
//! whole SSRF guard bypassed; so `wizard.http` calls
//! [`web_client`](crate::tools::web::web_client) and
//! [`get_following_redirects`](crate::tools::web::get_following_redirects)
//! rather than reqwest. And a second subagent spawner is a second place to get
//! the pane events, the read-only gate, the shared breaker and the
//! foreground/background cancellation split wrong; so `wizard.agent.spawn`
//! executes the `spawn_subagent` tool, which is the thing that already makes
//! all four of those decisions.
//!
//! # Why the live agent arrives through a slot
//!
//! There is one kernel per process ([`super::kernel`]) and therefore one host
//! behind every plugin's `wizard` table, but four of the five namespaces need
//! something only a running agent has: a provider, a token tracker, a cancel
//! handle, an event channel, a tool registry. The kernel is built long before
//! any of those exist — [`super::kernel`] runs from `llm::registry`, from
//! `wizard doctor`, from a unit test — so the host cannot be handed them at
//! construction.
//!
//! So [`WizardHost`] holds a slot that an agent fills through [`bind`]. What
//! this buys, and what it costs, are both worth stating plainly:
//!
//! - Unbound, the two namespaces that need no agent still work.
//!   `wizard.http` has the `[web]` defaults and `wizard.process` has the
//!   project root, which is exactly what a `wizard doctor` or a headless
//!   plugin-only process should get.
//! - Unbound, `wizard.model` and `wizard.agent` **refuse**, naming the reason.
//!   They could have fallen back to `Config::active().build()`, and that is
//!   precisely the wrong answer: a provider built on the side has no tracker
//!   behind it, so the spend would not reach `/cost`, and unmetered spend on
//!   the user's key is worse than a clear error.
//! - Unbound, `wizard.ui.notify` logs instead of refusing. A notice is the one
//!   call whose failure mode is nobody hearing it, and the log is a real place
//!   for a line nobody is watching for.
//! - Last binder wins. Two agents in one process — a fleet run, the gateway
//!   serving two sessions — share the slot, so a plugin's `wizard.model` bills
//!   whichever agent bound most recently. That is a real limitation and not a
//!   safe one to paper over; it is recorded here rather than hidden behind a
//!   handle that pretends otherwise.
//!
//! # Cancellation
//!
//! Every call that can block observes [`CancelHandle`], because the alternative
//! is that Ctrl-C returns the prompt while a plugin's fetch, completion or
//! child process keeps running. The handle comes off the bound
//! [`ToolContext`], which is the same handle the agent loop and the `execute`
//! tool watch, so one interrupt reaches all of them.
//!
//! The shapes differ by what is underneath. HTTP and the model call are
//! `tokio::select!` against [`crate::agent::cancelled`]: dropping a reqwest
//! future or a `ChatStream` is a clean abort. A child process is not — dropping
//! it reaps the shell and orphans whatever it forked — so `wizard.process` goes
//! through [`run_command_cancellable`](crate::tools::shell::run_command_cancellable),
//! which kills the process group from inside. The subagent path passes the
//! handle down as `SpawnOptions::cancel`, which is what a foreground
//! `spawn_subagent` already does.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use crate::agent::AgentEvent;
use crate::kernel::HostBridge;
use crate::llm::provider::LlmProvider;
use crate::llm::{ChatMessage, ChatRequest};
use crate::tools::{Tool, ToolContext};

/// The live agent behind `wizard.*`, as much of it as the host needs.
///
/// A snapshot rather than a handle on the `Agent`: an `Agent` is `!Sync` in
/// practice (it is driven by one task and mutated through `&mut self`) and the
/// host is called from every plugin VM at once. Everything here is either an
/// `Arc` or cheap to clone, and the agent re-binds when any of it changes.
pub struct Binding {
    /// Carries cwd, the `[web]` and `[shell]` slices, the cancel handle, the
    /// token tracker, the task and subagent registries, and — after the turn
    /// has started — the event channel.
    pub ctx: ToolContext,
    /// The agent's live provider. Re-bound on `/model` and `/fusion`, or a
    /// plugin answers from a model the user switched away from.
    pub client: Arc<dyn LlmProvider>,
    /// The tag `wizard.model.complete` sends.
    pub model: String,
    /// The registered `spawn_subagent` tool, when the agent has one.
    ///
    /// Taken from the agent's own registry rather than rebuilt, so a plugin's
    /// subagent is scoped from the same tools, hooks and configs the model's
    /// subagents are. `None` for a registry that does not carry it, which is
    /// every hand-assembled one in the tests.
    pub spawn: Option<Arc<dyn Tool>>,
}

/// The bridge a real Wizard process installs into its kernel.
pub struct WizardHost {
    /// Project root, for a `wizard.process.run` with no agent bound. The
    /// kernel's own root, so a command runs where a sandboxed plugin's file
    /// helpers are confined.
    root: PathBuf,
    bound: RwLock<Option<Arc<Binding>>>,
}

impl WizardHost {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            bound: RwLock::new(None),
        }
    }

    /// Replace the bound agent. See the module docs for why this is a slot.
    pub fn bind(&self, binding: Binding) {
        let mut slot = self
            .bound
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *slot = Some(Arc::new(binding));
    }

    fn binding(&self) -> Option<Arc<Binding>> {
        self.bound
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The tool context a host call runs under: the agent's, or a bare one at
    /// the kernel's project root.
    ///
    /// The bare one is not a degraded copy of the real one — it is exactly
    /// what a tool gets when it is executed outside an agent, with the same
    /// `[web]` and `[shell]` defaults — so the two paths differ in what they
    /// are allowed to reach and not in how they behave.
    fn context(&self) -> ToolContext {
        match self.binding() {
            Some(binding) => binding.ctx.clone(),
            None => ToolContext::new(self.root.clone()),
        }
    }
}

/// The refusal a namespace gives when it needs an agent and there is none.
fn unbound(table: &str, what: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "{table} needs a running agent to {what}, and no agent is attached to this process. \
         This is a plugin kernel without a session in front of it — `wizard doctor`, a \
         registry lookup, a test — not a misconfiguration."
    )
}

#[async_trait::async_trait]
impl HostBridge for WizardHost {
    /// One request, through the web tool's client and its SSRF guard.
    ///
    /// The body comes back as text rather than as the markdown `web_fetch`
    /// renders. A plugin fetching an endpoint wants the endpoint's answer —
    /// JSON, a header-shaped blob, a feed — and running that through an
    /// HTML-to-markdown converter would be the tool's presentation decision
    /// applied to a caller that is not presenting anything. Everything else is
    /// the tool's: `allow_local` decides whether loopback is reachable,
    /// `fetch_max_bytes` caps the body while it streams, and the result is
    /// defanged, because a plugin's answer usually ends up in front of a model
    /// and the control characters `defang` removes are exactly the ones a
    /// hostile page uses to talk to it.
    ///
    /// Redirects are followed for `GET` and refused for `POST`/`PUT`. Following
    /// a redirect with a body means replaying that body — very possibly a
    /// credential — to a host the plugin never named, which is the leak
    /// `send_following_redirects` was written to prevent for search backends.
    /// A plugin that means to post to the new location can read the status and
    /// do it.
    async fn http(&self, method: &str, url: &str, body: Option<String>) -> anyhow::Result<String> {
        let ctx = self.context();
        let cancel = ctx.cancel.clone();
        let allow_local = ctx.web.allow_local;
        let cap = ctx.web.fetch_max_bytes.max(1);

        let url: reqwest::Url = url
            .parse()
            .map_err(|err| anyhow::anyhow!("invalid url '{url}': {err}"))?;
        let method = method.to_ascii_uppercase();

        let fetch = async {
            crate::tools::web::check_url(&url, allow_local)
                .await
                .map_err(|err| anyhow::anyhow!("{err}"))?;
            let client = crate::tools::web::web_client()?;
            let response = match method.as_str() {
                "GET" => crate::tools::web::get_following_redirects(
                    &client,
                    url.clone(),
                    allow_local,
                    crate::tools::web::HopScheme::Any,
                )
                .await
                .map_err(|err| anyhow::anyhow!("{err}"))?,
                "POST" | "PUT" => {
                    let verb = if method == "POST" {
                        reqwest::Method::POST
                    } else {
                        reqwest::Method::PUT
                    };
                    let mut request = client.request(verb, url.clone());
                    if let Some(body) = body {
                        request = request.body(body);
                    }
                    let response = request.send().await?;
                    if response.status().is_redirection() {
                        anyhow::bail!(
                            "{method} {url} was redirected (HTTP {}); a body is not replayed to a \
                             host the plugin did not name",
                            response.status()
                        );
                    }
                    response
                }
                other => anyhow::bail!("wizard.http does not carry the {other} method"),
            };

            let status = response.status();
            let (bytes, capped) = crate::tools::web::read_capped(response, cap).await?;
            let mut text = String::from_utf8_lossy(&bytes).into_owned();
            if capped {
                text.push_str(&format!("\n... [response capped at {cap} bytes]"));
            }
            if !status.is_success() {
                anyhow::bail!(
                    "{method} {url} returned HTTP {status}: {}",
                    crate::tools::web::defang(&text)
                );
            }
            Ok(crate::tools::web::defang(&text))
        };

        tokio::select! {
            biased;
            () = crate::agent::cancelled(cancel.as_ref()) => Err(anyhow::anyhow!("interrupted")),
            fetched = fetch => fetched,
        }
    }

    /// One completion on the agent's provider, billed to the agent's tracker.
    ///
    /// The metering is not optional and is the reason this refuses without a
    /// binding: `model` is the capability `docs/plugins.md` describes as
    /// "spend tokens on the user's account", and a namespace that could spend
    /// without a tracker behind it would make `/cost` quietly wrong rather
    /// than loudly unavailable.
    ///
    /// No tools and no history: a plugin asking a model a question is asking a
    /// question, not starting a second agent inside the turn. A plugin that
    /// wants a loop with tools in it has `wizard.agent.spawn`, which is the
    /// call that carries the step budget, the deadline and the pane.
    async fn model(&self, plugin: &str, prompt: &str) -> anyhow::Result<String> {
        let Some(binding) = self.binding() else {
            return Err(unbound(
                "wizard.model",
                "spend tokens on the user's account",
            ));
        };
        let cancel = binding.ctx.cancel.clone();
        let usage = binding.ctx.usage.clone();
        let request = ChatRequest {
            model: binding.model.clone(),
            messages: vec![
                ChatMessage::system(format!(
                    "You are answering a question from the Wizard plugin '{plugin}'. \
                     Answer directly and concisely."
                )),
                ChatMessage::user(prompt.to_string()),
            ],
            tools: Vec::new(),
            stream: true,
            options: None,
        };

        let client = Arc::clone(&binding.client);
        let ask = async move {
            let stream = client.chat_stream(request).await?;
            crate::agent::ultra::collect_text_billed(stream, usage.as_deref()).await
        };

        tokio::select! {
            biased;
            () = crate::agent::cancelled(cancel.as_ref()) => Err(anyhow::anyhow!("interrupted")),
            answered = ask => answered,
        }
    }

    /// A line in the transcript, prefixed with the plugin that wrote it.
    ///
    /// The prefix is not decoration. A notice with no author is a line the
    /// user cannot attribute, and the first question about an unexpected one
    /// is always which plugin to disable.
    ///
    /// Falls back to the log when no surface is attached, and returns `Ok`
    /// either way. This is the one place where degrading beats refusing:
    /// [`UnwiredHost`](crate::kernel::UnwiredHost)'s argument against a silent
    /// no-op is that nobody can hear it, and a `tracing::info!` is somewhere it
    /// can be heard.
    async fn notify(&self, plugin: &str, text: &str) -> anyhow::Result<()> {
        let ctx = self.context();
        let line = format!("[{plugin}] {text}");
        match &ctx.events {
            Some(events) => {
                // A closed channel means the turn ended under the plugin, which
                // is not the plugin's fault and not worth an error it cannot
                // act on.
                let _ = events.send(AgentEvent::Notice(line)).await;
            }
            None => tracing::info!(plugin = %plugin, "{text}"),
        }
        Ok(())
    }

    /// One subagent, through the tool the model uses for the same thing.
    ///
    /// Executing `spawn_subagent` rather than calling
    /// [`crate::agent::subagent::spawn`] directly is deliberate: the tool is
    /// where the pane is announced, where the parent's live model is read off
    /// the shared binding, where the breaker is shared, and where the
    /// foreground/background cancellation split is decided. Reaching past it
    /// would mean making all four of those decisions again, in a second place,
    /// against a moving target.
    ///
    /// The `worker` config is the general-purpose builtin — the same one the
    /// model reaches for an unspecialised sub-task. Foreground, so the plugin's
    /// `await` means what it says and Ctrl-C ends it.
    async fn spawn_agent(&self, plugin: &str, task: &str) -> anyhow::Result<String> {
        let Some(binding) = self.binding() else {
            return Err(unbound("wizard.agent", "start a subagent"));
        };
        let Some(spawn) = binding.spawn.clone() else {
            anyhow::bail!(
                "wizard.agent needs the 'spawn_subagent' tool, and the agent bound to this \
                 process does not carry it"
            );
        };
        let args = serde_json::json!({
            "subagent": "worker",
            "task": format!("[requested by the '{plugin}' plugin] {task}"),
        });
        let output = spawn
            .execute(args, &binding.ctx)
            .await
            .map_err(anyhow::Error::new)?;
        if output.is_error {
            anyhow::bail!("{}", output.content);
        }
        Ok(output.content)
    }

    /// One command, through the shell tool's runner.
    ///
    /// The runner is what carries the parts that are easy to write and hard to
    /// write correctly: piped capture with a head/tail cap so a chatty command
    /// cannot exhaust memory, a drain grace so a forked grandchild's last bytes
    /// are not lost, its own process group so the timeout kill reaches the
    /// whole tree, and now the cancel handle so Ctrl-C does too.
    ///
    /// The budget is `[shell]`'s foreground budget, and a plugin's command is
    /// killed at it rather than detached. Detaching hands the child to the
    /// agent's background task registry, whose ids are announced to the *model*
    /// — a plugin has no way to ask about a task id and no way to be told one,
    /// so a detached command would be a process nobody could reach.
    async fn run(&self, plugin: &str, command: &str) -> anyhow::Result<String> {
        let ctx = self.context();
        let mut process = crate::platform::shell::tokio_command(command);
        process.current_dir(&ctx.cwd);
        let timeout = Duration::from_secs(ctx.shell.timeout_secs.max(1));
        let label = format!("wizard.process.run({plugin})");

        let result = crate::tools::shell::run_command_cancellable(
            &label,
            process,
            timeout,
            ctx.cancel.as_ref(),
        )
        .await
        .map_err(anyhow::Error::new)?;

        // Rendered here rather than through `render_command_result`, which
        // spills oversized output to a session file and rewrites the marker
        // into a "read this path" instruction addressed to the model. A plugin
        // is not the model and has nothing to read that path with.
        let mut out = result.stdout;
        if !result.stderr.trim().is_empty() {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&result.stderr);
        }
        if let Some(after) = result.timed_out {
            anyhow::bail!(
                "'{command}' was killed after {after}s.\n{}",
                crate::tools::truncate_output_without_spill(out, crate::tools::MAX_OUTPUT_BYTES)
            );
        }
        match result.code {
            Some(0) | None => Ok(crate::tools::truncate_output_without_spill(
                out,
                crate::tools::MAX_OUTPUT_BYTES,
            )),
            Some(code) => anyhow::bail!(
                "'{command}' exited {code}.\n{}",
                crate::tools::truncate_output_without_spill(out, crate::tools::MAX_OUTPUT_BYTES)
            ),
        }
    }
}

/// Bind the running agent to the process host bridge.
///
/// Called from the agent, not from a surface, for the same reason
/// [`super::boot`] is called from `crate::run`: `src/lib.rs` has seventeen
/// entrypoints and the ones that would be forgotten are the headless ones.
/// Every agent-bearing surface builds an [`crate::agent::Agent`]; binding
/// there covers all of them and costs a few `Arc` clones.
///
/// A no-op when the process host is not a [`WizardHost`], which is every
/// kernel a test builds for itself.
pub fn bind(binding: Binding) {
    if let Some(host) = super::host_bridge() {
        host.bind(binding);
    }
}

#[cfg(test)]
mod tests;
