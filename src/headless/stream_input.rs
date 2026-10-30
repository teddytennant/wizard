//! `--input-format stream-json`: a conversation over stdin and stdout.
//!
//! Each `{"type":"user"}` line is one turn, answered with the same lines a
//! one-shot run prints (`system` init, `assistant` / `user` messages) and a
//! `result`. `control_request` lines are answered with a `control_response`:
//! `initialize` and `interrupt` do what they say, `set_model` switches the
//! model, and anything else is refused by name. The input ends the run.

use std::collections::VecDeque;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

use super::support;
use crate::agent::{Agent, AgentEvent, DoneReason};
use crate::cli::Cli;
use crate::config::Config;
use crate::output::{EventSink, OutputFormat, StreamJsonSink};

/// The text of a `{"type":"user"}` line: a string, or its text blocks.
fn user_text(message: &Value) -> Option<String> {
    let content = message.get("message")?.get("content")?;
    let text = match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    (!text.trim().is_empty()).then_some(text)
}

fn control_ok(request_id: &str, response: Value) -> Value {
    json!({
        "type": "control_response",
        "response": {"subtype": "success", "request_id": request_id, "response": response},
    })
}

fn control_error(request_id: &str, error: &str) -> Value {
    json!({
        "type": "control_response",
        "response": {"subtype": "error", "request_id": request_id, "error": error},
    })
}

/// Answer one control request. `interrupt` is handled by the caller, which
/// owns the cancel handle; this is everything that can wait for the turn to
/// end.
async fn answer_control(
    request: &Value,
    agent: &mut Agent,
    custom: &[crate::commands::CustomCommand],
) -> Value {
    let id = request
        .get("request_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let body = request.get("request").unwrap_or(&Value::Null);
    match body.get("subtype").and_then(Value::as_str) {
        Some("initialize") => control_ok(
            id,
            json!({
                "commands": custom.iter().map(|c| json!({
                    "name": c.name,
                    "description": c.description,
                })).collect::<Vec<_>>(),
                "output_style": crate::claude_dir::styles::active_name(
                    &std::env::current_dir().unwrap_or_default()
                ),
                "models": [],
            }),
        ),
        Some("set_model") => match body.get("model").and_then(Value::as_str) {
            Some(model) => {
                let native =
                    crate::llm::provider::probe_native_tools(agent.client().as_ref(), model).await;
                agent.set_model(model.to_string(), native);
                control_ok(id, json!({}))
            }
            None => control_error(id, "set_model needs a model"),
        },
        Some("interrupt") => control_ok(id, json!({})),
        Some(other) => control_error(id, &format!("'{other}' is not supported by Wizard")),
        None => control_error(id, "a control request needs a subtype"),
    }
}

pub(super) async fn run(mut config: Config, cli: Cli) -> Result<i32> {
    anyhow::ensure!(
        cli.output_format == OutputFormat::StreamJson,
        "--input-format=stream-json requires output-format=stream-json."
    );
    let project_root = std::env::current_dir().context("determining project root")?;
    let custom = crate::commands::load(&project_root);
    let (mut agent, mut fallbacks) = support::open_agent(&mut config, &project_root, &cli).await?;

    let info = crate::claude_stream::info::describe(
        &config,
        &project_root,
        &agent.session().id,
        &agent.tool_names(),
        crate::claude_stream::info::StreamFlags::from_flags(&cli.claude),
    );
    let mut sink = Some(StreamJsonSink::stdout().with_info(info, Some(agent.cancel_handle())));

    let (inbox_tx, mut inbox) = mpsc::unbounded_channel::<Value>();
    tokio::spawn(async move {
        let mut lines = BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(value) = serde_json::from_str::<Value>(&line) {
                let _ = inbox_tx.send(value);
            }
        }
    });

    let (boot_tx, mut boot_rx) = mpsc::channel::<AgentEvent>(64);
    agent.fire_session_start(&boot_tx).await;
    drop(boot_tx);
    while boot_rx.try_recv().is_ok() {}

    let cancel = agent.cancel_handle();
    let trip = support::Trip::new(cancel.clone());
    let mut queue: VecDeque<Value> = VecDeque::new();
    let mut exit = 0;
    loop {
        let message = match queue.pop_front() {
            Some(message) => message,
            None => match inbox.recv().await {
                Some(message) => message,
                None => break,
            },
        };
        let kind = message.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == "control_request" {
            let reply = answer_control(&message, &mut agent, &custom).await;
            if let Some(sink) = sink.as_mut() {
                sink.emit(reply);
            }
            continue;
        }
        if kind != "user" {
            continue;
        }
        let Some(text) = user_text(&message) else {
            continue;
        };
        let prepared = crate::commands::preprocess(&text, &custom, &project_root);

        let mut turn_sink = sink.take().context("the output sink went missing")?;
        turn_sink.start();
        if cli.claude.replay_user_messages {
            let mut echo = message.clone();
            if let Some(object) = echo.as_object_mut() {
                object.insert("session_id".into(), json!(turn_sink.session_id()));
                object.insert("parent_tool_use_id".into(), Value::Null);
                object.insert("isReplay".into(), json!(true));
            }
            turn_sink.emit(echo);
        }
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
        let watch = trip.clone();
        let printer = tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                watch.observe(&event);
                turn_sink.event(event);
            }
            turn_sink
        });
        let outcome = {
            let turn = support::run_with_fallback(
                &mut agent,
                &prepared.text,
                prepared.images,
                &tx,
                &mut fallbacks,
                &trip,
            );
            tokio::pin!(turn);
            loop {
                tokio::select! {
                    result = &mut turn => break result,
                    Some(next) = inbox.recv() => {
                        let interrupt = next.get("type").and_then(Value::as_str)
                            == Some("control_request")
                            && next.pointer("/request/subtype").and_then(Value::as_str)
                                == Some("interrupt");
                        if interrupt {
                            cancel.cancel();
                        }
                        queue.push_back(next);
                    }
                }
            }
        };
        drop(tx);
        let mut returned = printer.await.context("the output task ended abnormally")?;
        let reason = match outcome {
            Ok(reason) => reason,
            Err(err) => {
                returned.event(AgentEvent::Error(format!("{err:#}")));
                exit = 1;
                DoneReason::CircuitBreaker
            }
        };
        returned.finish_turn(reason);
        let over_budget = returned.budget_exceeded();
        sink = Some(returned);
        if over_budget {
            exit = 1;
            break;
        }
    }
    agent.fire_session_end(None).await;
    support::discard_session(&agent);
    Ok(exit)
}
