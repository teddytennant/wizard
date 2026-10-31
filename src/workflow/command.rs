//! `/workflows`: list the runs and the saved workflows, show one run, stop one.

use std::fmt::Write as _;
use std::path::Path;

use super::board;
use super::live;
use super::store::{self, RunRecord};

const USAGE: &str = "/workflows [list] | /workflows <run id> | /workflows stop <run id>";

fn ago(from_ms: u64) -> String {
    let secs = store::now_ms().saturating_sub(from_ms) / 1000;
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

fn took(record: &RunRecord) -> String {
    let end = record.ended_ms.unwrap_or_else(store::now_ms);
    let secs = end.saturating_sub(record.started_ms) / 1000;
    if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

pub fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        let cut: String = flat.chars().take(max.saturating_sub(3)).collect();
        format!("{cut}...")
    }
}

/// The mark the plain-text views put before an agent in this state.
pub fn glyph(state: board::AgentState) -> &'static str {
    use board::AgentState::*;
    match state {
        Queued => "-",
        Running => "*",
        Done | Cached => "+",
        Failed => "x",
        Stopped => "~",
    }
}

fn agent_took(agent: &board::AgentRow) -> String {
    match (agent.started_ms, agent.ended_ms) {
        (Some(start), Some(end)) => format!("  {}s", end.saturating_sub(start) / 1000),
        (Some(start), None) => format!("  {}s", store::now_ms().saturating_sub(start) / 1000),
        _ => String::new(),
    }
}

fn list(cwd: &Path) -> String {
    let runs = store::list();
    let mut out = String::new();
    if runs.is_empty() {
        out.push_str("no workflow runs yet\n");
    } else {
        out.push_str("workflow runs, newest first\n");
        for run in runs.iter().take(20) {
            let _ = writeln!(
                out,
                "  {}  {:<11} {:<24} {} agent{}  {}  {}",
                run.run_id,
                store::effective_status(run),
                one_line(&run.name, 24),
                run.agents,
                if run.agents == 1 { " " } else { "s" },
                took(run),
                ago(run.started_ms),
            );
        }
        if runs.len() > 20 {
            let _ = writeln!(out, "  ... and {} older", runs.len() - 20);
        }
    }
    let (named, problems) = store::discover(cwd, dirs::home_dir().as_deref());
    if !named.is_empty() {
        out.push_str("\nsaved workflows\n");
        for workflow in &named {
            let _ = writeln!(
                out,
                "  {:<24} {} ({})",
                workflow.name,
                one_line(&workflow.meta.description, 60),
                workflow.origin.label()
            );
        }
    }
    for problem in problems {
        let _ = writeln!(out, "  could not load {problem}");
    }
    let _ = write!(out, "\n{USAGE}");
    out
}

fn show(id: &str) -> Result<String, String> {
    let id = store::resolve_id(id)?;
    let run = store::load(&id).map_err(|e| format!("{e:#}"))?;
    let dir = store::run_dir(&id).map_err(|e| format!("{e:#}"))?;
    let mut out = String::new();
    let _ = writeln!(out, "{}  {}", run.run_id, run.name);
    let _ = writeln!(out, "  {}", one_line(&run.description, 100));
    let _ = writeln!(
        out,
        "  status: {}  took {}  started {}",
        store::effective_status(&run),
        took(&run),
        ago(run.started_ms)
    );
    let _ = writeln!(
        out,
        "  agents: {} ({} cached)  source: {}",
        run.agents, run.cached, run.source
    );
    if !run.phases.is_empty() {
        let _ = writeln!(out, "  phases: {}", run.phases.join(", "));
    }
    if let Some(board) = board::read(&id) {
        for agent in board.agents.iter().take(30) {
            let _ = writeln!(
                out,
                "    {} {:<28} {}{}",
                glyph(agent.state),
                one_line(&agent.label, 28),
                agent.phase.as_deref().unwrap_or("-"),
                agent_took(agent),
            );
        }
        if board.agents.len() > 30 {
            let _ = writeln!(out, "    ... and {} more agents", board.agents.len() - 30);
        }
    }
    if !run.args.is_null() {
        let _ = writeln!(out, "  args: {}", one_line(&run.args.to_string(), 100));
    }
    if let Some(error) = &run.error {
        let _ = writeln!(out, "  error: {}", one_line(error, 200));
    }
    for failure in run.failures.iter().take(5) {
        let _ = writeln!(out, "  failed: {}", one_line(failure, 160));
    }
    if run.failures.len() > 5 {
        let _ = writeln!(out, "  ... and {} more failures", run.failures.len() - 5);
    }
    if let Some(result) = &run.result {
        let _ = writeln!(out, "  result: {}", one_line(&result.to_string(), 300));
    }
    let _ = write!(
        out,
        "  script: {}\n  journal: {}\n  resume: Workflow({{scriptPath, resumeFromRunId: \"{}\"}})",
        dir.join("script.js").display(),
        dir.join("journal.jsonl").display(),
        run.run_id
    );
    Ok(out)
}

fn stop(id: &str) -> Result<String, String> {
    let id = store::resolve_id(id)?;
    if live::stop(&id) {
        return Ok(format!("stopping {id}"));
    }
    let run = store::load(&id).map_err(|e| format!("{e:#}"))?;
    if store::effective_status(&run) == "running" {
        board::request_stop(&id).map_err(|e| format!("{e:#}"))?;
        return Ok(format!(
            "asked process {} to stop {id}; it checks every second",
            run.pid
        ));
    }
    Err(format!(
        "{id} is not running ({})",
        store::effective_status(&run)
    ))
}

/// Run `/workflows <args>`.
pub fn run(args: &str, cwd: &Path) -> Result<String, String> {
    let args = args.trim();
    let (head, rest) = args
        .split_once(char::is_whitespace)
        .map_or((args, ""), |(h, r)| (h, r.trim()));
    match head {
        "" | "list" | "ls" => Ok(list(cwd)),
        "stop" | "kill" => stop(rest),
        "show" | "open" => show(rest),
        id => show(id),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::workflow::store::Status;

    fn record(status: Status, pid: u32) -> RunRecord {
        RunRecord {
            run_id: store::new_run_id(),
            name: "review-changes".into(),
            description: "Review changed files".into(),
            status,
            started_ms: store::now_ms(),
            ended_ms: None,
            cwd: ".".into(),
            source: "inline".into(),
            args: Value::Null,
            agents: 4,
            cached: 1,
            phases: vec!["Review".into()],
            result: Some(json!({"confirmed": []})),
            error: None,
            failures: vec!["agent 2 [x]: boom".into()],
            pid,
            task_id: Some(3),
        }
    }

    #[test]
    fn list_show_and_stop_read_the_store() {
        let done = record(Status::Completed, std::process::id());
        store::create(&done, "x").unwrap();
        let cwd = std::env::temp_dir();

        let listing = run("", &cwd).unwrap();
        assert!(listing.contains(&done.run_id), "{listing}");
        assert!(listing.contains("completed") && listing.contains("review-changes"));
        assert!(listing.contains("/workflows stop <run id>"));

        let shown = run(&done.run_id, &cwd).unwrap();
        assert!(shown.contains("agents: 4 (1 cached)"), "{shown}");
        assert!(shown.contains("failed: agent 2 [x]: boom"));
        assert!(shown.contains("resumeFromRunId"));
        assert_eq!(
            run(&format!("show {}", &done.run_id[3..9]), &cwd).unwrap(),
            shown
        );

        let err = run(&format!("stop {}", done.run_id), &cwd).unwrap_err();
        assert!(err.contains("is not running (completed)"), "{err}");
        assert!(run("stop wf_nothere", &cwd).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn a_run_of_another_process_is_stopped_with_a_file() {
        // The test runner's parent is alive and is not this process.
        let theirs = record(Status::Running, std::os::unix::process::parent_id());
        store::create(&theirs, "x").unwrap();
        let said = run(&format!("stop {}", theirs.run_id), &std::env::temp_dir()).unwrap();
        assert!(said.contains("to stop"), "{said}");
        assert!(board::stop_requested(&theirs.run_id));
    }

    #[test]
    fn a_live_run_is_stopped_and_a_dead_one_reads_interrupted() {
        let cancel = live::Cancel::new();
        let running = record(Status::Running, std::process::id());
        store::create(&running, "x").unwrap();
        live::register(&running.run_id, cancel.clone());
        let cwd = std::env::temp_dir();
        assert!(
            run(&running.run_id, &cwd)
                .unwrap()
                .contains("status: running")
        );
        assert_eq!(
            run(&format!("stop {}", running.run_id), &cwd).unwrap(),
            format!("stopping {}", running.run_id)
        );
        assert!(cancel.is_cancelled());
        live::unregister(&running.run_id);

        // Marked running, owned by a process that is gone.
        let ghost = record(Status::Running, 4_000_000);
        store::create(&ghost, "x").unwrap();
        assert!(
            run(&ghost.run_id, &cwd)
                .unwrap()
                .contains("status: interrupted")
        );
    }
}
