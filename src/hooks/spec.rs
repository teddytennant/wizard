//! What a hook is: an event, a matcher, and one of five handlers.
//!
//! Two file formats feed this. `hooks.toml` is Wizard's own and has always
//! been `event`, `matcher`, `command`, `timeout_secs`; it keeps working
//! unchanged and gains optional keys for the other handler types. A Claude
//! Code `settings.json` nests the same thing as event, matcher group, hook
//! list. Both end up as the same [`HookSpec`].

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Map, Value};

use super::HookDef;
use super::event::HookEvent;

/// Where a hook came from. It decides tool naming in the payload and how
/// stdout is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// `hooks.toml`.
    Native,
    /// A Claude-format settings file.
    Claude,
}

/// How a hook does its work.
#[derive(Debug, Clone, PartialEq)]
pub enum Handler {
    /// A shell command line, or `command` plus `args` run without a shell.
    Command {
        command: String,
        args: Option<Vec<String>>,
    },
    /// POST the payload as JSON to a URL; the response body is the output.
    Http {
        url: String,
        headers: BTreeMap<String, String>,
        allowed_env_vars: Vec<String>,
    },
    /// One model call that answers `{"ok": bool, "reason": "..."}`.
    Prompt {
        prompt: String,
        model: Option<String>,
    },
    /// A read-only subagent that investigates, then answers the same way.
    Agent {
        prompt: String,
        model: Option<String>,
    },
    /// Call a tool on a connected MCP server.
    McpTool {
        server: String,
        tool: String,
        input: Value,
    },
}

impl Handler {
    /// Short text for logs and the `HookFired` event.
    pub fn describe(&self) -> String {
        match self {
            Handler::Command { command, .. } => command.clone(),
            Handler::Http { url, .. } => format!("http {url}"),
            Handler::Prompt { prompt, .. } => format!("prompt {}", clip(prompt)),
            Handler::Agent { prompt, .. } => format!("agent {}", clip(prompt)),
            Handler::McpTool { server, tool, .. } => format!("mcp {server}/{tool}"),
        }
    }

    /// Default wall-clock budget in seconds, by handler type.
    pub fn default_timeout(&self) -> u64 {
        match self {
            Handler::Prompt { .. } => 30,
            Handler::Agent { .. } => 60,
            _ => super::DEFAULT_TIMEOUT_SECS,
        }
    }
}

fn clip(text: &str) -> String {
    let line = text.lines().next().unwrap_or("");
    line.chars().take(48).collect()
}

/// One configured hook.
#[derive(Debug, Clone, PartialEq)]
pub struct HookSpec {
    pub event: HookEvent,
    /// Glob for [`Origin::Native`], Claude matcher syntax for
    /// [`Origin::Claude`].
    pub matcher: Option<String>,
    pub handler: Handler,
    pub timeout_secs: Option<u64>,
    pub origin: Origin,
    /// Read stdout as the JSON decision protocol. Always on for Claude
    /// hooks and for events added after the original six.
    pub json: bool,
    /// One permission rule (`Bash(git *)`) the call must match for the hook
    /// to run. Tool events only.
    pub condition: Option<String>,
    /// Run without waiting; the result is ignored.
    pub is_async: bool,
    /// Treat a failure (spawn error, timeout, exit other than 0 or 2) as a
    /// block instead of a warning.
    pub block_on_failure: bool,
    /// File the hook was read from, for `/hooks`-style listings.
    pub source: String,
}

impl From<HookDef> for HookSpec {
    fn from(def: HookDef) -> Self {
        HookSpec {
            json: !is_original_event(def.event),
            event: def.event,
            matcher: def.matcher,
            handler: Handler::Command {
                command: def.command,
                args: None,
            },
            timeout_secs: def.timeout_secs,
            origin: Origin::Native,
            condition: None,
            is_async: false,
            block_on_failure: false,
            source: "hooks.toml".to_string(),
        }
    }
}

/// The six events `hooks.toml` had before Claude parity. Their plain-stdout
/// behavior is frozen.
pub fn is_original_event(event: HookEvent) -> bool {
    matches!(
        event,
        HookEvent::PreToolUse
            | HookEvent::PostToolUse
            | HookEvent::UserPromptSubmit
            | HookEvent::SessionStart
            | HookEvent::SessionEnd
            | HookEvent::TurnEnd
    )
}

/// On-disk `[[hooks]]` entry with every optional key.
#[derive(Debug, Deserialize)]
struct RawHook {
    event: HookEvent,
    #[serde(default)]
    matcher: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Option<Vec<String>>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    allowed_env_vars: Vec<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    server: Option<String>,
    #[serde(default)]
    tool: Option<String>,
    #[serde(default)]
    input: Option<toml::Value>,
    /// `"json"` turns on the decision protocol for one of the six original
    /// events.
    #[serde(default)]
    protocol: Option<String>,
    #[serde(default, rename = "if")]
    condition: Option<String>,
    #[serde(default, rename = "async")]
    is_async: bool,
    #[serde(default)]
    on_failure: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawFile {
    #[serde(default)]
    hooks: Vec<RawHook>,
}

/// Parse a `hooks.toml`. An entry whose handler is incomplete (a `command`
/// hook with no command) is dropped with a warning.
pub fn parse_toml(raw: &str, source: &str) -> Result<Vec<HookSpec>, toml::de::Error> {
    let file: RawFile = toml::from_str(raw)?;
    let mut specs = Vec::new();
    for hook in file.hooks {
        let kind = hook.kind.clone().unwrap_or_else(|| "command".to_string());
        let input = hook
            .input
            .as_ref()
            .and_then(|value| serde_json::to_value(value).ok())
            .unwrap_or(Value::Null);
        let handler = build_handler(
            &kind,
            HandlerFields {
                command: hook.command.clone(),
                args: hook.args.clone(),
                url: hook.url.clone(),
                headers: hook.headers.clone(),
                allowed_env_vars: hook.allowed_env_vars.clone(),
                prompt: hook.prompt.clone(),
                model: hook.model.clone(),
                server: hook.server.clone(),
                tool: hook.tool.clone(),
                input,
            },
        );
        let Some(handler) = handler else {
            tracing::warn!(
                "{source}: dropping a {kind} hook on {} with missing fields",
                hook.event.name()
            );
            continue;
        };
        specs.push(HookSpec {
            json: !is_original_event(hook.event) || hook.protocol.as_deref() == Some("json"),
            event: hook.event,
            matcher: hook.matcher,
            handler,
            timeout_secs: hook.timeout_secs,
            origin: Origin::Native,
            condition: hook.condition,
            is_async: hook.is_async,
            block_on_failure: hook.on_failure.as_deref() == Some("block"),
            source: source.to_string(),
        });
    }
    Ok(specs)
}

struct HandlerFields {
    command: Option<String>,
    args: Option<Vec<String>>,
    url: Option<String>,
    headers: BTreeMap<String, String>,
    allowed_env_vars: Vec<String>,
    prompt: Option<String>,
    model: Option<String>,
    server: Option<String>,
    tool: Option<String>,
    input: Value,
}

fn build_handler(kind: &str, f: HandlerFields) -> Option<Handler> {
    match kind {
        "command" => Some(Handler::Command {
            command: f.command.filter(|c| !c.trim().is_empty())?,
            args: f.args,
        }),
        "http" => Some(Handler::Http {
            url: f.url.filter(|u| !u.trim().is_empty())?,
            headers: f.headers,
            allowed_env_vars: f.allowed_env_vars,
        }),
        "prompt" => Some(Handler::Prompt {
            prompt: f.prompt.filter(|p| !p.trim().is_empty())?,
            model: f.model,
        }),
        "agent" => Some(Handler::Agent {
            prompt: f.prompt.filter(|p| !p.trim().is_empty())?,
            model: f.model,
        }),
        "mcp_tool" => Some(Handler::McpTool {
            server: f.server?,
            tool: f.tool?,
            input: f.input,
        }),
        _ => None,
    }
}

/// Read the `hooks` object of a Claude-format settings file. An event name
/// this build does not know is skipped without invalidating the file.
pub fn from_claude_settings(hooks: &Value, source: &str) -> Vec<HookSpec> {
    let Some(events) = hooks.as_object() else {
        return Vec::new();
    };
    let mut specs = Vec::new();
    for (name, groups) in events {
        let Some(event) = HookEvent::from_claude_name(name) else {
            tracing::debug!("{source}: unknown hook event {name}, skipped");
            continue;
        };
        for group in groups.as_array().into_iter().flatten() {
            let matcher = group
                .get("matcher")
                .and_then(Value::as_str)
                .map(str::to_string);
            for hook in group
                .get("hooks")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(spec) = claude_hook(event, matcher.clone(), hook, source) {
                    specs.push(spec);
                }
            }
        }
    }
    specs
}

fn claude_hook(
    event: HookEvent,
    matcher: Option<String>,
    hook: &Value,
    source: &str,
) -> Option<HookSpec> {
    let text = |key: &str| hook.get(key).and_then(Value::as_str).map(str::to_string);
    let kind = text("type").unwrap_or_else(|| "command".to_string());
    let headers: BTreeMap<String, String> = hook
        .get("headers")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let strings = |key: &str| -> Option<Vec<String>> {
        hook.get(key).and_then(Value::as_array).map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
    };
    let handler = build_handler(
        &kind,
        HandlerFields {
            command: text("command"),
            args: strings("args"),
            url: text("url"),
            headers,
            allowed_env_vars: strings("allowedEnvVars").unwrap_or_default(),
            prompt: text("prompt"),
            model: text("model"),
            server: text("server"),
            tool: text("tool"),
            input: hook
                .get("input")
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new())),
        },
    );
    let Some(handler) = handler else {
        tracing::warn!(
            "{source}: skipping a {kind} hook on {} with missing fields",
            event.claude_name()
        );
        return None;
    };
    Some(HookSpec {
        event,
        matcher,
        handler,
        timeout_secs: hook.get("timeout").and_then(Value::as_u64),
        origin: Origin::Claude,
        json: true,
        condition: text("if"),
        is_async: hook.get("async").and_then(Value::as_bool).unwrap_or(false),
        block_on_failure: hook.get("onFailure").and_then(Value::as_str) == Some("block"),
        source: source.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn claude_settings_nest_event_matcher_and_handlers() {
        let hooks = json!({
            "PreToolUse": [{
                "matcher": "Bash",
                "hooks": [
                    {"type": "command", "command": "check.sh", "timeout": 5, "if": "Bash(git *)"},
                    {"type": "http", "url": "https://example.com/hook", "headers": {"X-A": "$TOKEN"}, "allowedEnvVars": ["TOKEN"]},
                    {"type": "prompt", "prompt": "is this safe? $ARGUMENTS"},
                    {"type": "mcp_tool", "server": "audit", "tool": "log", "input": {"msg": "${tool_name}"}}
                ]
            }],
            "Stop": [{"hooks": [{"type": "agent", "prompt": "verify the tests ran", "model": "fast"}]}],
            "FutureEvent": [{"hooks": [{"type": "command", "command": "x"}]}]
        });
        let specs = from_claude_settings(&hooks, "settings.json");
        assert_eq!(specs.len(), 5, "the unknown event is skipped");
        let pre: Vec<_> = specs
            .iter()
            .filter(|s| s.event == HookEvent::PreToolUse)
            .collect();
        assert_eq!(pre.len(), 4);
        assert_eq!(pre[0].matcher.as_deref(), Some("Bash"));
        assert_eq!(pre[0].timeout_secs, Some(5));
        assert_eq!(pre[0].condition.as_deref(), Some("Bash(git *)"));
        assert!(matches!(pre[1].handler, Handler::Http { .. }));
        assert!(matches!(pre[2].handler, Handler::Prompt { .. }));
        assert!(matches!(&pre[3].handler, Handler::McpTool { server, .. } if server == "audit"));
        assert!(specs.iter().all(|s| s.origin == Origin::Claude && s.json));
    }

    #[test]
    fn a_handler_missing_its_fields_is_dropped_not_fatal() {
        let hooks = json!({"Stop": [{"hooks": [
            {"type": "command"},
            {"type": "http"},
            {"type": "nonsense", "command": "x"},
            {"type": "command", "command": "ok"}
        ]}]});
        let specs = from_claude_settings(&hooks, "s");
        assert_eq!(specs.len(), 1);
    }

    #[test]
    fn exec_form_args_are_kept() {
        let hooks = json!({"Stop": [{"hooks": [
            {"type": "command", "command": "/bin/echo", "args": ["a", "b"]}
        ]}]});
        let specs = from_claude_settings(&hooks, "s");
        assert!(matches!(
            &specs[0].handler,
            Handler::Command { args: Some(args), .. } if args == &["a", "b"]
        ));
    }

    #[test]
    fn toml_without_new_keys_is_what_it_always_was() {
        let specs = parse_toml(
            "[[hooks]]\nevent = \"pre_tool_use\"\nmatcher = \"execute\"\ncommand = \"c\"\ntimeout_secs = 3\n",
            "hooks.toml",
        )
        .unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].origin, Origin::Native);
        assert!(
            !specs[0].json,
            "original events keep the frozen stdout rules"
        );
        assert_eq!(specs[0].timeout_secs, Some(3));
    }

    #[test]
    fn toml_new_events_and_handlers() {
        let specs = parse_toml(
            r#"
            [[hooks]]
            event = "stop"
            type = "prompt"
            prompt = "did the tests run?"

            [[hooks]]
            event = "pre_tool_use"
            protocol = "json"
            type = "http"
            url = "http://localhost:9/hook"

            [[hooks]]
            event = "notification"
            type = "mcp_tool"
            server = "chat"
            tool = "post"
            input = { text = "${message}" }

            [[hooks]]
            event = "cwd_changed"
            "#,
            "hooks.toml",
        )
        .unwrap();
        assert_eq!(specs.len(), 3, "the last has no handler and is dropped");
        assert!(specs[0].json && matches!(specs[0].handler, Handler::Prompt { .. }));
        assert!(specs[1].json, "protocol = json opts an original event in");
        assert!(
            matches!(&specs[2].handler, Handler::McpTool { input, .. } if input["text"] == "${message}")
        );
    }

    #[test]
    fn hook_def_converts_with_frozen_semantics() {
        let spec: HookSpec = HookDef {
            event: HookEvent::SessionStart,
            matcher: None,
            command: "c".into(),
            timeout_secs: None,
        }
        .into();
        assert!(!spec.json);
        let spec: HookSpec = HookDef {
            event: HookEvent::Stop,
            matcher: None,
            command: "c".into(),
            timeout_secs: None,
        }
        .into();
        assert!(spec.json);
    }
}
