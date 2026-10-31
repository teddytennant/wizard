use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;

use super::*;
use crate::workflow::engine::{AgentOutcome, AgentRunner, Limits, Quiet};
use crate::workflow::live::Cancel;
use crate::workflow::store::JournalEntry;

/// Answers `echo:<prompt>`, or the schema's `{ok: true}`, and remembers requests.
#[derive(Default)]
struct Mock {
    seen: Mutex<Vec<AgentRequest>>,
}

#[async_trait]
impl AgentRunner for Mock {
    async fn run(&self, request: AgentRequest) -> Result<AgentOutcome, String> {
        self.seen.lock().unwrap().push(request.clone());
        if request.prompt.contains("hang") {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
        if request.prompt.contains("die") {
            return Ok(AgentOutcome::Failed("boom".into()));
        }
        if request.schema.is_some() {
            return Ok(AgentOutcome::Done(json!({"ok": true, "n": request.index})));
        }
        Ok(AgentOutcome::Done(json!(format!(
            "echo:{}",
            request.prompt
        ))))
    }
}

fn limits() -> Limits {
    Limits {
        max_agents: 100,
        concurrency: 4,
    }
}

fn no_children() -> Resolver {
    Arc::new(|_| Err("no such workflow".to_string()))
}

fn engine_with(mock: Arc<Mock>) -> Arc<Engine> {
    Arc::new(Engine::new(
        "wf_vmtest00-000",
        mock,
        limits(),
        Arc::new(Quiet),
        Cancel::new(),
    ))
}

async fn exec(body: &str, args: Value) -> (Result<Value, String>, Arc<Mock>, Arc<Engine>) {
    let mock = Arc::new(Mock::default());
    let engine = engine_with(Arc::clone(&mock));
    let out = run(Arc::clone(&engine), no_children(), body, &args).await;
    (out, mock, engine)
}

#[tokio::test]
async fn the_body_runs_with_args_and_returns_json() {
    let (out, _, _) = exec(
        "return { sum: args.a + args.b, list: [1, 'x', null] }",
        json!({"a": 2, "b": 3}),
    )
    .await;
    assert_eq!(out.unwrap(), json!({"sum": 5, "list": [1, "x", null]}));
    let (out, _, _) = exec("return undefined", Value::Null).await;
    assert_eq!(out.unwrap(), Value::Null);
}

#[tokio::test]
async fn agent_returns_the_runners_text_and_numbers_calls_in_script_order() {
    let (out, mock, _) = exec(
        "const a = agent('one'); const b = agent('two', {label: 'second'}); return [await b, await a]",
        Value::Null,
    )
    .await;
    assert_eq!(out.unwrap(), json!(["echo:two", "echo:one"]));
    let seen = mock.seen.lock().unwrap();
    let mut by_index: Vec<(u32, String)> =
        seen.iter().map(|r| (r.index, r.label.clone())).collect();
    by_index.sort();
    assert_eq!(
        by_index,
        [(1, "one".to_string()), (2, "second".to_string())]
    );
}

#[tokio::test]
async fn parallel_keeps_order_and_turns_a_throw_into_null() {
    let (out, _, engine) = exec(
        "return await parallel([() => agent('a'), () => { throw new Error('nope') }, () => agent('c')])",
        Value::Null,
    )
    .await;
    assert_eq!(out.unwrap(), json!(["echo:a", null, "echo:c"]));
    assert!(
        engine.failures()[0].contains("parallel[1]: nope"),
        "{:?}",
        engine.failures()
    );
}

#[tokio::test]
async fn parallel_wants_functions_not_promises() {
    let (out, _, _) = exec("return await parallel([agent('a')])", Value::Null).await;
    let err = out.unwrap_err();
    assert!(
        err.contains("parallel() expects an array of functions, not promises"),
        "{err}"
    );
}

#[tokio::test]
async fn pipeline_passes_previous_item_and_index_and_drops_a_throwing_item() {
    let (out, _, _) = exec(
        "return await pipeline(['x', 'y', 'z'], (item, orig, i) => agent(item + i), (prev, orig, i) => { if (orig === 'y') throw new Error('bad'); return prev + '|' + orig + i })",
        Value::Null,
    )
    .await;
    assert_eq!(out.unwrap(), json!(["echo:x0|x0", null, "echo:z2|z2"]));
}

#[tokio::test]
async fn pipeline_rejects_non_function_stages_and_non_arrays() {
    let (out, _, _) = exec("return await pipeline([1], 5)", Value::Null).await;
    assert!(
        out.unwrap_err()
            .contains("pipeline() stages must be functions")
    );
    let (out, _, _) = exec("return await pipeline('no', x => x)", Value::Null).await;
    assert!(
        out.unwrap_err()
            .contains("pipeline() expects an array as the first argument")
    );
}

#[tokio::test]
async fn more_than_4096_items_is_an_error_not_a_truncation() {
    let (out, _, _) = exec(
        "return await parallel(new Array(4097).fill(() => 1))",
        Value::Null,
    )
    .await;
    assert!(out.unwrap_err().contains("at most 4096"));
}

#[tokio::test]
async fn a_failed_agent_is_null_to_the_script() {
    let (out, _, engine) = exec("return await agent('please die')", Value::Null).await;
    assert_eq!(out.unwrap(), Value::Null);
    assert!(engine.failures()[0].contains("boom"));
}

#[tokio::test]
async fn phase_and_log_reach_the_engine_and_label_later_agents() {
    let (out, mock, engine) = exec(
        "phase('Scan'); log('hello'); await agent('a'); phase('Fix'); await agent('b', {phase: 'Own'}); return 1",
        Value::Null,
    )
    .await;
    out.unwrap();
    assert_eq!(engine.phases(), ["Scan", "Fix"]);
    let seen = mock.seen.lock().unwrap();
    assert_eq!(seen[0].phase.as_deref(), Some("Scan"));
    assert_eq!(seen[1].phase.as_deref(), Some("Own"));
}

#[tokio::test]
async fn the_clock_and_dice_throw() {
    for source in ["Date.now()", "new Date()", "Math.random()", "Date()"] {
        let (out, _, _) = exec(&format!("return {source}"), Value::Null).await;
        let err = out.unwrap_err();
        assert!(
            err.contains("unavailable in workflow scripts"),
            "{source}: {err}"
        );
    }
    let (out, _, _) = exec("return new Date(86400000).toISOString()", Value::Null).await;
    assert_eq!(out.unwrap(), json!("1970-01-02T00:00:00.000Z"));
}

#[tokio::test]
async fn a_bad_schema_throws_before_the_agent_starts() {
    let (out, mock, _) = exec(
        "try { await agent('x', {schema: {type: 'array'}}); return 'ran' } catch (e) { return e.message }",
        Value::Null,
    )
    .await;
    assert!(
        out.unwrap()
            .as_str()
            .unwrap()
            .contains("{type: 'object', properties")
    );
    assert!(mock.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_schema_reaches_the_runner_and_the_object_comes_back() {
    let (out, mock, _) = exec(
        "const r = await agent('x', {schema: {type: 'object', properties: {ok: {type: 'boolean'}}}}); return r",
        Value::Null,
    )
    .await;
    assert_eq!(out.unwrap(), json!({"ok": true, "n": 1}));
    assert!(mock.seen.lock().unwrap()[0].schema.is_some());
}

#[tokio::test]
async fn options_are_normalised() {
    let (out, mock, _) = exec(
        "await agent('  spaced   out\\nprompt ', {model: 'm', agentType: 'reviewer', isolation: 'worktree', disallowedTools: ['Write', 'Bash', 'Write']}); return 1",
        Value::Null,
    )
    .await;
    out.unwrap();
    let request = mock.seen.lock().unwrap()[0].clone();
    assert_eq!(request.label, "spaced out prompt");
    assert_eq!(request.model.as_deref(), Some("m"));
    assert_eq!(request.agent_type.as_deref(), Some("reviewer"));
    assert!(request.worktree);
    assert_eq!(request.disallowed_tools, ["Bash", "Write"]);
}

#[tokio::test]
async fn unsupported_isolation_is_refused() {
    let (out, _, _) = exec(
        "return await agent('x', {isolation: 'remote'})",
        Value::Null,
    )
    .await;
    assert!(out.unwrap_err().contains("only 'worktree'"));
}

#[tokio::test]
async fn workflow_runs_a_child_once_and_children_cannot_nest() {
    let resolver: Resolver = Arc::new(|reference| {
        match reference["name"].as_str() {
        Some("child") => Ok((
            "child".to_string(),
            "phase('inner'); const r = await agent('from child ' + args.n); return {r, name: typeof workflow}".to_string(),
        )),
        Some("greedy") => Ok((
            "greedy".to_string(),
            "return await workflow('child', {n: 9})".to_string(),
        )),
        _ => Err("Workflow \"x\" not found. Available: child".to_string()),
    }
    });
    let mock = Arc::new(Mock::default());
    let engine = engine_with(Arc::clone(&mock));
    let out = run(
        Arc::clone(&engine),
        Arc::clone(&resolver),
        "const c = await workflow('child', {n: 7}); let nested; try { await workflow('greedy') } catch (e) { nested = e.message } let missing; try { await workflow('x') } catch (e) { missing = e.message } return {c, nested, missing}",
        &Value::Null,
    )
    .await
    .unwrap();
    assert_eq!(out["c"]["r"], json!("echo:from child 7"));
    assert!(
        out["nested"].as_str().unwrap().contains("one level only"),
        "{out}"
    );
    assert!(out["missing"].as_str().unwrap().contains("not found"));
    let seen = mock.seen.lock().unwrap();
    assert_eq!(seen[0].phase.as_deref(), Some("\u{25B8} child: inner"));
    // The child's agents count against the parent's run.
    assert_eq!(engine.calls(), 1);
}

// Multi-threaded: a spinning script holds its thread, and the stop has to come
// from another one, as it does in a session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_ends_a_script_waiting_on_an_agent_and_one_that_spins() {
    let mock = Arc::new(Mock::default());
    let engine = engine_with(Arc::clone(&mock));
    let cancel = Arc::clone(&engine.cancel);
    let task = tokio::spawn({
        let engine = Arc::clone(&engine);
        async move {
            run(
                engine,
                no_children(),
                "return await agent('hang')",
                &Value::Null,
            )
            .await
        }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    cancel.cancel();
    let out = tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .expect("stopped")
        .unwrap();
    assert_eq!(out.unwrap_err(), "Workflow aborted");

    let engine = engine_with(Arc::new(Mock::default()));
    let cancel = Arc::clone(&engine.cancel);
    let spin = tokio::spawn({
        let engine = Arc::clone(&engine);
        async move { run(engine, no_children(), "while (true) {}", &Value::Null).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    cancel.cancel();
    let out = tokio::time::timeout(Duration::from_secs(3), spin)
        .await
        .expect("stopped")
        .unwrap();
    assert!(out.is_err());
}

#[tokio::test]
async fn resume_through_the_script_runs_only_what_changed() {
    let first = Arc::new(Mock::default());
    let engine = engine_with(Arc::clone(&first));
    let script = "const a = await agent('one'); const b = await agent('two'); return [a, b]";
    run(Arc::clone(&engine), no_children(), script, &Value::Null)
        .await
        .unwrap();
    let journal: Vec<JournalEntry> = first
        .seen
        .lock()
        .unwrap()
        .iter()
        .map(|r| JournalEntry {
            i: r.index,
            key: crate::workflow::store::call_key(
                &r.prompt,
                &json!({"schema": null, "model": null, "agentType": null, "worktree": false, "disallowedTools": []}),
            ),
            label: r.label.clone(),
            phase: None,
            ok: true,
            result: json!(format!("echo:{}", r.prompt)),
            error: None,
        })
        .collect();

    let second = Arc::new(Mock::default());
    let engine = Arc::new(
        Engine::new(
            "wf_vmtest01-000",
            second.clone(),
            limits(),
            Arc::new(Quiet),
            Cancel::new(),
        )
        .resume_from(journal),
    );
    let edited =
        "const a = await agent('one'); const b = await agent('two, edited'); return [a, b]";
    let out = run(Arc::clone(&engine), no_children(), edited, &Value::Null)
        .await
        .unwrap();
    assert_eq!(out, json!(["echo:one", "echo:two, edited"]));
    assert_eq!(second.seen.lock().unwrap().len(), 1);
    assert_eq!(engine.cached(), 1);
}

#[tokio::test]
async fn a_function_result_and_a_throw_are_errors() {
    let (out, _, _) = exec("return () => 1", Value::Null).await;
    assert_eq!(out.unwrap_err(), "workflow result cannot be a function");
    let (out, _, _) = exec("throw new Error('kaput')", Value::Null).await;
    assert!(out.unwrap_err().contains("kaput"));
}

#[tokio::test]
async fn syntax_errors_are_reported_without_running() {
    assert!(check_syntax("const x = 1; return x").await.is_ok());
    let err = check_syntax("const x: number = 1").await.unwrap_err();
    assert!(err.starts_with("Script parse error:"), "{err}");
    assert!(err.contains("plain JavaScript"), "{err}");
}

#[tokio::test]
async fn there_is_no_way_out_of_the_vm() {
    let (out, _, _) = exec(
        "return [typeof require, typeof process, typeof fetch, typeof std, typeof os, typeof XMLHttpRequest]",
        Value::Null,
    )
    .await;
    assert_eq!(out.unwrap(), json!(vec!["undefined"; 6]));
}
