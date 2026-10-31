use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::json;

use super::*;
use crate::workflow::engine::{AgentOutcome, AgentRequest};

#[derive(Default)]
struct Mock {
    prompts: Mutex<Vec<String>>,
}

#[async_trait]
impl AgentRunner for Mock {
    async fn run(&self, request: AgentRequest) -> Result<AgentOutcome, String> {
        self.prompts.lock().unwrap().push(request.prompt.clone());
        if request.prompt.contains("hang") {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
        Ok(AgentOutcome::Done(json!(format!("got {}", request.prompt))))
    }
}

fn deps(mock: &Arc<Mock>, ctx: &ToolContext) -> Deps {
    Deps {
        runner: mock.clone(),
        ctx: ctx.clone(),
        limits: Limits {
            max_agents: 50,
            concurrency: 4,
        },
    }
}

fn inline(script: &str) -> Input {
    Input {
        script: Some(script.to_string()),
        ..Input::default()
    }
}

const TWO_AGENTS: &str = "export const meta = {name: 'two', description: 'two agents', phases: [{title: 'Go'}]}\nphase('Go')\nconst a = await agent('first')\nconst b = await agent('second ' + args.tag)\nreturn {a, b}\n";

async fn launched(input: Input, mock: &Arc<Mock>, ctx: &ToolContext) -> Launched {
    let resolved = resolve(&input, &ctx.cwd, None).unwrap();
    validate(&resolved).await.unwrap();
    launch(deps(mock, ctx), &input, resolved).await.unwrap()
}

#[tokio::test]
async fn a_run_completes_records_itself_and_reaches_the_task_registry() {
    let ctx = ToolContext::new(std::env::temp_dir());
    let mock = Arc::new(Mock::default());
    let mut input = inline(TWO_AGENTS);
    input.args = Some(json!({"tag": "x"}));
    let run = launched(input, &mock, &ctx).await;
    assert!(store::valid_run_id(&run.run_id));
    let outcome = run.done.await.unwrap();

    assert_eq!(outcome.status, Status::Completed);
    assert_eq!(
        outcome.result,
        Some(json!({"a": "got first", "b": "got second x"}))
    );
    let record = store::load(&run.run_id).unwrap();
    assert_eq!(record.status, Status::Completed);
    assert_eq!((record.agents, record.name.as_str()), (2, "two"));
    assert!(record.phases.contains(&"Go".to_string()));
    assert_eq!(store::read_journal(&run.run_id).len(), 2);
    assert!(run.dir.join("result.json").exists());
    assert!(!live::is_live(&run.run_id));

    // The task registry shows it finished, with the result as the tail.
    let finished = ctx.tasks.drain_completed();
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].id, run.task_id);
    assert_eq!(finished[0].status, TaskStatus::Done(0));
    assert!(
        finished[0].tail.contains("completed in"),
        "{}",
        finished[0].tail
    );
    assert!(finished[0].tail.contains("got second x"));
    let (_, output) = ctx.tasks.output(run.task_id, 10_000).unwrap();
    assert!(output.contains("phase: Go"), "{output}");
    assert!(output.contains("agent 1 [first] done"), "{output}");
}

#[tokio::test]
async fn a_thrown_error_fails_the_run_with_the_message() {
    let ctx = ToolContext::new(std::env::temp_dir());
    let mock = Arc::new(Mock::default());
    let run = launched(
        inline("export const meta = {name: 'bad', description: 'd'}\nthrow new Error('kaput')"),
        &mock,
        &ctx,
    )
    .await;
    let outcome = run.done.await.unwrap();
    assert_eq!(outcome.status, Status::Failed);
    assert!(outcome.error.unwrap().contains("kaput"));
    let finished = ctx.tasks.drain_completed();
    assert_eq!(finished[0].status, TaskStatus::Done(1));
    assert!(finished[0].tail.contains("failed after"));
}

#[tokio::test]
async fn task_stop_stops_the_run_and_the_record_says_stopped() {
    let ctx = ToolContext::new(std::env::temp_dir());
    let mock = Arc::new(Mock::default());
    let run = launched(
        inline("export const meta = {name: 'slow', description: 'd'}\nreturn await agent('hang')"),
        &mock,
        &ctx,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(live::is_live(&run.run_id));
    assert_eq!(ctx.tasks.status(run.task_id), Some(TaskStatus::Running));
    assert!(ctx.tasks.kill(run.task_id));
    let outcome = tokio::time::timeout(Duration::from_secs(5), run.done)
        .await
        .expect("stopped")
        .unwrap();
    assert_eq!(outcome.status, Status::Stopped);
    assert_eq!(store::load(&run.run_id).unwrap().status, Status::Stopped);
    assert_eq!(ctx.tasks.status(run.task_id), Some(TaskStatus::Killed));
    assert!(!live::is_live(&run.run_id));
}

#[tokio::test]
async fn resume_reuses_the_run_and_only_reruns_the_edited_agent() {
    let ctx = ToolContext::new(std::env::temp_dir());
    let first = Arc::new(Mock::default());
    let mut input = inline(TWO_AGENTS);
    input.args = Some(json!({"tag": "x"}));
    let run = launched(input, &first, &ctx).await;
    run.done.await.unwrap();

    let second = Arc::new(Mock::default());
    let mut input = inline(TWO_AGENTS);
    input.args = Some(json!({"tag": "CHANGED"}));
    input.resume_from_run_id = Some(run.run_id.clone());
    let again = launched(input, &second, &ctx).await;
    assert_eq!(again.run_id, run.run_id);
    let outcome = again.done.await.unwrap();
    assert_eq!(
        outcome.result,
        Some(json!({"a": "got first", "b": "got second CHANGED"}))
    );
    assert_eq!(*second.prompts.lock().unwrap(), ["second CHANGED"]);
    let record = store::load(&run.run_id).unwrap();
    assert_eq!((record.agents, record.cached), (2, 1));
    assert_eq!(store::read_journal(&run.run_id).len(), 2);
}

#[tokio::test]
async fn resume_is_refused_for_a_running_run_and_for_a_missing_journal() {
    let ctx = ToolContext::new(std::env::temp_dir());
    let mock = Arc::new(Mock::default());
    let run = launched(
        inline("export const meta = {name: 'slow', description: 'd'}\nreturn await agent('hang')"),
        &mock,
        &ctx,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut input = inline(TWO_AGENTS);
    input.resume_from_run_id = Some(run.run_id.clone());
    let resolved = resolve(&input, &ctx.cwd, None).unwrap();
    let err = launch(deps(&mock, &ctx), &input, resolved)
        .await
        .err()
        .unwrap();
    assert!(err.contains("is still running"), "{err}");
    assert!(err.contains("TaskStop"), "{err}");
    ctx.tasks.kill(run.task_id);
    let _ = run.done.await;

    let mut input = inline(TWO_AGENTS);
    input.resume_from_run_id = Some("wf_00000000-000".to_string());
    let resolved = resolve(&input, &ctx.cwd, None).unwrap();
    let err = launch(deps(&mock, &ctx), &input, resolved)
        .await
        .err()
        .unwrap();
    assert!(err.contains("nothing to resume"), "{err}");
}

#[tokio::test]
async fn resolve_prefers_path_then_name_then_script_and_explains_misses() {
    let dir = std::env::temp_dir().join(format!("wf-resolve-{}", uuid::Uuid::new_v4()));
    let wf = dir.join(".wizard/workflows");
    std::fs::create_dir_all(&wf).unwrap();
    let file = "export const meta = {name: 'saved', description: 'a saved one'}\nreturn 1";
    std::fs::write(wf.join("saved.js"), file).unwrap();

    let by_name = resolve(
        &Input {
            name: Some("saved".into()),
            script: Some("ignored".into()),
            ..Input::default()
        },
        &dir,
        None,
    )
    .unwrap();
    assert_eq!(by_name.source, "name:saved");

    let by_path = resolve(
        &Input {
            script_path: Some(".wizard/workflows/saved.js".into()),
            name: Some("other".into()),
            ..Input::default()
        },
        &dir,
        None,
    )
    .unwrap();
    assert!(by_path.source.starts_with("path:"));

    let missing = resolve(
        &Input {
            name: Some("nope".into()),
            ..Input::default()
        },
        &dir,
        None,
    )
    .unwrap_err();
    assert_eq!(
        missing,
        "Workflow \"nope\" not found. Available: saved, deep-research, code-review, batch"
    );

    let gone = resolve(
        &Input {
            script_path: Some("nope.js".into()),
            ..Input::default()
        },
        &dir,
        None,
    )
    .unwrap_err();
    assert_eq!(gone, "Workflow script file not found: nope.js");

    assert_eq!(
        resolve(&Input::default(), &dir, None).unwrap_err(),
        "Must provide script, name, or scriptPath"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn validate_refuses_a_syntax_error_and_the_clock() {
    let ctx = ToolContext::new(std::env::temp_dir());
    let bad = resolve(
        &inline("export const meta = {name: 'a', description: 'b'}\nconst x: number = 1"),
        &ctx.cwd,
        None,
    )
    .unwrap();
    let err = validate(&bad).await.unwrap_err();
    assert!(
        err.starts_with("Invalid workflow script: Script parse error:"),
        "{err}"
    );
    assert!(err.ends_with("fix the script, and retry."), "{err}");

    let clock = resolve(
        &inline("export const meta = {name: 'a', description: 'b'}\nreturn Date.now()"),
        &ctx.cwd,
        None,
    )
    .unwrap();
    let err = validate(&clock).await.unwrap_err();
    assert!(err.starts_with(script::NONDETERMINISM), "{err}");
    assert!(err.contains("Load the `workflow-authoring` skill"), "{err}");
}

#[tokio::test]
async fn a_run_writes_its_board_and_a_stop_file_stops_it() {
    let ctx = ToolContext::new(std::env::temp_dir());
    let mock = Arc::new(Mock::default());
    let run = launched(
        inline(
            "export const meta = {name: 'panel', description: 'd', phases: [{title: 'Go'}]}\nphase('Go')\nconst a = await agent('quick', {label: 'q', phase: 'Go'})\nreturn await agent('hang', {label: 'h', phase: 'Go'})",
        ),
        &mock,
        &ctx,
    )
    .await;
    let deadline = Instant::now() + Duration::from_secs(5);
    let board = loop {
        if let Some(board) = board::read(&run.run_id)
            && board.agents.len() == 2
            && board.agents[1].state == board::AgentState::Running
        {
            break board;
        }
        assert!(Instant::now() < deadline, "no board written");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(board.phases, ["Go"]);
    assert_eq!(board.agents[0].state, board::AgentState::Done);
    assert_eq!(board.agents[0].outcome.as_deref(), Some("got quick"));
    assert_eq!(board.agents[1].label, "h");
    assert_eq!(board.phase_progress("Go"), (1, 2));

    board::request_stop(&run.run_id).unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(5), run.done)
        .await
        .expect("the stop file ends the run")
        .unwrap();
    assert_eq!(outcome.status, Status::Stopped);
    let after = board::read(&run.run_id).unwrap();
    assert_eq!(after.agents[1].state, board::AgentState::Stopped);
}
