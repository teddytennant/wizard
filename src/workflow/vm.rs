//! The QuickJS side: one VM per run, with `agent()`, `parallel()`, `pipeline()`,
//! `phase()`, `log()`, `workflow()`, `args` and `budget` installed.
//!
//! The functions a script sees are written in JavaScript (`prelude.js`) on top
//! of a handful of host functions defined here. The VM has no filesystem, no
//! network and no module loader; the only way out is `host.agent`, which goes
//! through the [`Engine`]. The interrupt handler stops a script that spins
//! without awaiting when the run is stopped.

use std::sync::Arc;

use rquickjs::prelude::Async;
use rquickjs::{AsyncContext, AsyncRuntime, Ctx as JsCtx, Exception, Function, Object};
use serde_json::{Value, json};

use super::engine::{AgentRequest, Engine, Pending};
use crate::kernel::js::convert::js_to_json;

const PRELUDE: &str = include_str!("prelude.js");

/// Memory a run's VM may hold. Agent results live in it until the script drops them.
const MEMORY_LIMIT: usize = 512 * 1024 * 1024;

const NOT_PLAIN: &str = "Workflow scripts must be plain JavaScript \u{2014} common causes are TypeScript syntax (type annotations, interfaces, generics) and broken string quoting or escaping.";

/// Turns `workflow(ref)` into `(name, body)`: the child script, parsed.
pub type Resolver = Arc<dyn Fn(&Value) -> Result<(String, String), String> + Send + Sync>;

fn thrown(js: &JsCtx<'_>, err: rquickjs::Error) -> String {
    if !matches!(err, rquickjs::Error::Exception) {
        return err.to_string();
    }
    let caught = js.catch();
    if let Some(exception) = caught.as_exception() {
        // The message only: the stack's frames name the prelude's line numbers,
        // which mean nothing to a script's author.
        let message = exception.message().unwrap_or_default();
        return if message.is_empty() {
            "an uncaught exception".to_string()
        } else {
            message
        };
    }
    match js_to_json(&caught) {
        Ok(Value::String(text)) => text,
        Ok(json) => json.to_string(),
        Err(_) => "an uncaught exception".to_string(),
    }
}

fn fail<'js>(js: &JsCtx<'js>, message: &str) -> rquickjs::Error {
    Exception::throw_message(js, message)
}

fn option_str(opts: &Value, key: &str) -> Option<String> {
    opts.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// A script's `agent(prompt, opts)` as an [`AgentRequest`], or why it is refused.
fn request_of(prompt: String, opts: &Value) -> Result<AgentRequest, String> {
    let schema = match opts.get("schema") {
        None | Some(Value::Null) => None,
        Some(schema) => {
            super::schema::check(schema)?;
            Some(schema.clone())
        }
    };
    let worktree = match opts.get("isolation").and_then(Value::as_str) {
        None => false,
        Some("worktree") => true,
        Some(other) => {
            return Err(format!(
                "agent() isolation {other:?} is not supported here; only 'worktree' is"
            ));
        }
    };
    let disallowed_tools = match opts.get("disallowedTools") {
        None => Vec::new(),
        Some(Value::Array(items)) => {
            let mut names = Vec::new();
            for item in items {
                match item.as_str() {
                    Some(name) if !name.is_empty() && name == name.trim() => {
                        names.push(name.to_string());
                    }
                    _ => {
                        return Err("agent() opts.disallowedTools must be an array of non-empty tool-name strings (e.g. ['Bash', 'Write']). Refusing the spawn rather than running it un-narrowed.".into());
                    }
                }
            }
            names.sort();
            names.dedup();
            names
        }
        Some(_) => {
            return Err("agent() opts.disallowedTools must be an array of non-empty tool-name strings (e.g. ['Bash', 'Write']). Refusing the spawn rather than running it un-narrowed.".into());
        }
    };
    let label = option_str(opts, "label")
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .unwrap_or_else(|| {
            let flat = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
            flat.chars().take(60).collect()
        });
    Ok(AgentRequest {
        index: 0,
        label,
        phase: option_str(opts, "phase"),
        prompt,
        schema,
        model: option_str(opts, "model"),
        agent_type: option_str(opts, "agentType"),
        worktree,
        disallowed_tools,
    })
}

fn install_host<'js>(
    js: &JsCtx<'js>,
    engine: &Arc<Engine>,
    resolver: &Resolver,
) -> rquickjs::Result<Object<'js>> {
    let host = Object::new(js.clone())?;

    let agent_engine = Arc::clone(engine);
    host.set(
        "agent",
        Function::new(
            js.clone(),
            Async(move |cx: JsCtx<'js>, prompt: String, opts: String| {
                // Everything before `async move` runs when the script calls
                // `agent()`, which is what numbers the call in script order.
                let opts: Value = serde_json::from_str(&opts).unwrap_or(Value::Null);
                let pending = match request_of(prompt, &opts) {
                    Ok(request) => agent_engine.begin(request),
                    Err(why) => Pending::Refused(why),
                };
                let engine = Arc::clone(&agent_engine);
                async move {
                    match engine.finish(pending).await {
                        Ok(value) => Ok(json!({ "v": value }).to_string()),
                        Err(why) => Err(fail(&cx, &why)),
                    }
                }
            }),
        )?,
    )?;

    let phase_engine = Arc::clone(engine);
    host.set(
        "phase",
        Function::new(js.clone(), move |title: String| phase_engine.phase(&title))?,
    )?;
    let log_engine = Arc::clone(engine);
    host.set(
        "log",
        Function::new(js.clone(), move |line: String| log_engine.log(&line))?,
    )?;
    let fail_engine = Arc::clone(engine);
    host.set(
        "fail",
        Function::new(js.clone(), move |line: String| fail_engine.fail(&line))?,
    )?;
    let spent_engine = Arc::clone(engine);
    host.set(
        "spent",
        Function::new(js.clone(), move || spent_engine.spent() as f64)?,
    )?;

    let resolve = Arc::clone(resolver);
    host.set(
        "loadWorkflow",
        Function::new(js.clone(), move |cx: JsCtx<'js>, reference: String| {
            let reference: Value = serde_json::from_str(&reference).unwrap_or(Value::Null);
            resolve(&reference)
                .map(|(name, body)| json!({ "name": name, "body": body }).to_string())
                .map_err(|why| fail(&cx, &why))
        })?,
    )?;
    Ok(host)
}

fn plain_error(message: &str) -> String {
    format!("Script parse error: {message}\n{NOT_PLAIN}")
}

/// Compile `body` without running it, to report a syntax error before a launch.
pub async fn check_syntax(body: &str) -> Result<(), String> {
    let runtime = AsyncRuntime::new().map_err(|e| format!("creating the QuickJS runtime: {e}"))?;
    let context = AsyncContext::full(&runtime)
        .await
        .map_err(|e| format!("creating the QuickJS context: {e}"))?;
    let body = body.to_string();
    context
        .async_with(async |js| {
            let outcome: rquickjs::Result<()> = (|| {
                let factory: Function = js.eval(PRELUDE)?;
                let host = Object::new(js.clone())?;
                let api: Object = factory.call((host,))?;
                let check: Function = api.get("check")?;
                check.call::<_, ()>((body.as_str(),))
            })();
            outcome.map_err(|err| plain_error(&thrown(&js, err)))
        })
        .await
}

/// Run `body` as the workflow and return what it returns, as JSON.
pub async fn run(
    engine: Arc<Engine>,
    resolver: Resolver,
    body: &str,
    args: &Value,
) -> Result<Value, String> {
    let runtime = AsyncRuntime::new().map_err(|e| format!("creating the QuickJS runtime: {e}"))?;
    runtime.set_memory_limit(MEMORY_LIMIT).await;
    let cancel = Arc::clone(&engine.cancel);
    runtime
        .set_interrupt_handler(Some(Box::new(move || cancel.is_cancelled())))
        .await;
    let context = AsyncContext::full(&runtime)
        .await
        .map_err(|e| format!("creating the QuickJS context: {e}"))?;
    let body = body.to_string();
    let args_json = serde_json::to_string(args).unwrap_or_else(|_| "null".into());
    let cancel = Arc::clone(&engine.cancel);

    let work = context.async_with(async |js| {
        let started: rquickjs::Result<rquickjs::Promise> = (|| {
            let host = install_host(&js, &engine, &resolver)?;
            let factory: Function = js.eval(PRELUDE)?;
            let api: Object = factory.call((host,))?;
            let run: Function = api.get("run")?;
            run.call((body.as_str(), args_json.as_str()))
        })();
        let promise = started.map_err(|err| thrown(&js, err))?;
        let text = promise
            .into_future::<String>()
            .await
            .map_err(|err| thrown(&js, err))?;
        serde_json::from_str::<Value>(&text).map_err(|e| format!("the result is not JSON: {e}"))
    });
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err("Workflow aborted".to_string()),
        outcome = work => outcome,
    }
}

#[cfg(test)]
#[path = "vm_tests.rs"]
mod tests;
