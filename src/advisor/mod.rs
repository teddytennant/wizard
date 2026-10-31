//! The advisor: a second, usually stronger, model the working agent can ask
//! while it is mid-task.
//!
//! Claude Code's advisor is a server-side tool: the API itself runs the second
//! model over the whole transcript and returns an opaque block. Wizard cannot
//! decode that block and has no server to run it, so this is the same idea done
//! with what it has. The `advisor` tool takes the question and a summary the
//! working model writes of where it is, sends both to the model named by the
//! `advisorModel` setting as one plain completion, and returns the guidance as
//! the tool result. The advisor sees only that summary, has no tools and never
//! acts; the working model stays the only thing that edits.
//!
//! `/fusion` and `/ultra` are the other multi-model forms: they run on every
//! step or on request and merge answers. The advisor is asked once, on the
//! model's own initiative, and costs one call.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::config::{Config, ProviderConfig};
use crate::llm::provider::LlmProvider;
use crate::llm::{ChatMessage, ChatRequest};

/// The settings key, shared with Claude Code.
pub const SETTING: &str = "advisorModel";

/// Environment override, which `--advisor` publishes.
pub const ENV: &str = "WIZARD_ADVISOR_MODEL";

/// One consultation is a single completion; this bounds a stalled endpoint.
const TIMEOUT: Duration = Duration::from_secs(300);

const SYSTEM: &str = "You are the advisor to another coding agent. It is partway through a task \
and has stopped to ask you. You cannot run tools or read files; you see only the summary it \
wrote. Answer in under 400 words with concrete guidance: the approach you would take, the \
mistakes you would watch for, and what to check before calling the work done. If the summary \
leaves out something you need, say what, instead of guessing. Do not write the whole solution.";

/// The advisor model the user chose, if any: the environment first, then
/// `advisorModel` in `~/.wizard/settings.json`.
pub fn configured() -> Option<String> {
    std::env::var(ENV)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .or_else(|| crate::usersettings::get_str(SETTING))
}

/// Where a spec points: the provider to call and the model to ask it for.
#[derive(Debug, Clone)]
pub struct Target {
    pub provider: ProviderConfig,
    pub model: String,
    /// How the spec was read, for the `/advisor` reply.
    pub how: String,
}

impl Target {
    pub fn label(&self) -> String {
        format!("{}:{}", self.provider.name, self.model)
    }
}

/// Read `spec` against the configured providers.
///
/// A provider name uses that provider's own model; `name/model` or
/// `name:model` asks that provider for another one; a model tag some provider
/// is configured with picks that provider; anything else is a model tag asked
/// of the active provider.
pub fn resolve(spec: &str, providers: &[ProviderConfig], active: &ProviderConfig) -> Target {
    let spec = spec.trim();
    if let Some(p) = providers.iter().find(|p| p.name == spec) {
        return Target {
            model: p.model.clone(),
            provider: p.clone(),
            how: format!("provider '{}' with its own model", p.name),
        };
    }
    for sep in ['/', ':'] {
        if let Some((name, model)) = spec.split_once(sep)
            && let Some(p) = providers.iter().find(|p| p.name == name)
            && !model.is_empty()
        {
            return Target {
                model: model.to_string(),
                provider: p.clone(),
                how: format!("model '{model}' on provider '{}'", p.name),
            };
        }
    }
    if let Some(p) = providers.iter().find(|p| p.model == spec) {
        return Target {
            model: p.model.clone(),
            provider: p.clone(),
            how: format!("the model provider '{}' is configured with", p.name),
        };
    }
    Target {
        model: spec.to_string(),
        provider: active.clone(),
        how: format!(
            "no provider is named '{spec}' or configured with that model, so it is asked of the active provider '{}'",
            active.name
        ),
    }
}

/// The target the saved spec names, using the loaded config.
pub fn target(spec: &str) -> Result<Target, String> {
    let config = Config::load().map_err(|err| format!("{err:#}"))?;
    Ok(resolve(spec, &config.providers, &config.active()))
}

/// What the working agent hands the advisor.
pub fn brief(question: &str, context: &str) -> String {
    if context.trim().is_empty() {
        format!("Question:\n{question}")
    } else {
        format!("Where the work stands:\n{context}\n\nQuestion:\n{question}")
    }
}

/// One completion against `client`, billed to `usage` when given.
pub async fn consult(
    client: Arc<dyn LlmProvider>,
    model: &str,
    question: &str,
    context: &str,
    usage: Option<&crate::usage::UsageTracker>,
) -> Result<String, String> {
    let request = ChatRequest {
        model: model.to_string(),
        messages: vec![
            ChatMessage::system(SYSTEM),
            ChatMessage::user(brief(question, context)),
        ],
        tools: Vec::new(),
        stream: true,
        options: None,
    };
    let ask = async {
        let stream = client.chat_stream(request).await?;
        crate::agent::ultra::collect_text_billed(stream, usage).await
    };
    match tokio::time::timeout(TIMEOUT, ask).await {
        Err(_) => Err(format!(
            "the advisor did not answer within {}s",
            TIMEOUT.as_secs()
        )),
        Ok(Err(err)) => Err(format!("the advisor call failed: {err:#}")),
        Ok(Ok(text)) if text.trim().is_empty() => {
            Err("the advisor answered with nothing".to_string())
        }
        Ok(Ok(text)) => Ok(text.trim().to_string()),
    }
}

/// The `advisor` tool's body: resolve the saved spec, build its provider, ask.
pub async fn consult_configured(
    question: &str,
    context: &str,
    usage: Option<&crate::usage::UsageTracker>,
) -> Result<String, String> {
    let Some(spec) = configured() else {
        return Err(
            "no advisor is set. The user can name one with /advisor <model>, or <provider>/<model> \
             for one of their configured providers; /advisor off clears it."
                .to_string(),
        );
    };
    let target = target(&spec)?;
    let client = target
        .provider
        .build()
        .map_err(|err| format!("building provider '{}': {err:#}", target.provider.name))?;
    let answer = consult(client, &target.model, question, context, usage).await?;
    Ok(format!("Advisor ({}) says:\n\n{answer}", target.label()))
}

/// `/advisor [<model>|off]`.
pub fn command(args: &str) -> Result<String, String> {
    let args = args.trim();
    match args {
        "" => Ok(match configured() {
            None => {
                "Advisor: off. Usage: /advisor <model>|off, where the model is a provider name, \
                     <provider>/<model>, or a model tag. The agent can then call the `advisor` \
                     tool for a second opinion: one plain completion over the summary the agent \
                     writes. For a standing panel see /fusion and /ultra."
                    .to_string()
            }
            Some(spec) => match target(&spec) {
                Ok(t) => format!(
                    "Advisor: {spec} ({}: {}). /advisor off clears it.",
                    t.label(),
                    t.how
                ),
                Err(err) => format!("Advisor: {spec} (not resolvable now: {err})"),
            },
        }),
        "off" | "none" | "unset" | "clear" => {
            let path = crate::usersettings::set(SETTING, None)?;
            Ok(format!(
                "Advisor disabled. ({SETTING} removed from {}.){}",
                path.display(),
                if std::env::var_os(ENV).is_some() {
                    format!(" {ENV} is still set in this environment and wins.")
                } else {
                    String::new()
                }
            ))
        }
        spec => {
            let t = target(spec)?;
            let path = crate::usersettings::set(SETTING, Some(Value::String(spec.to_string())))?;
            Ok(format!(
                "Advisor set to {spec}: {} ({}). Saved as {SETTING} in {}; the agent can call the \
                 `advisor` tool from now on.",
                t.label(),
                t.how,
                path.display()
            ))
        }
    }
}

#[cfg(test)]
mod tests;
