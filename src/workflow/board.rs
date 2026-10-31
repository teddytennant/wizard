//! What a run is doing right now: each agent's state, grouped by phase.
//!
//! The engine keeps a [`Board`] while it runs, and the launcher writes it to
//! `agents.json` in the run's directory about once a second. Readers (the
//! terminal panel, the Claude look, `/workflows <id>`) read that file, so a run
//! started by another process shows the same as one started by this one. A
//! stop is a file too: [`request_stop`] writes `stop` in the run's directory
//! and the run's own process notices it on its next tick.

use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::store;

/// Longest prompt or outcome kept per agent; the journal has the whole result.
const KEPT_CHARS: usize = 1500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    /// Called, waiting for a slot.
    Queued,
    Running,
    Done,
    /// Answered from the journal of an earlier run.
    Cached,
    Failed,
    /// The run was stopped before this agent finished.
    Stopped,
}

impl AgentState {
    pub fn finished(self) -> bool {
        !matches!(self, AgentState::Queued | AgentState::Running)
    }
}

/// One `agent()` call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentRow {
    /// 1-based position among the run's calls.
    pub i: u32,
    pub label: String,
    pub phase: Option<String>,
    pub state: AgentState,
    /// The subagent that runs it.
    pub agent: String,
    pub started_ms: Option<u64>,
    pub ended_ms: Option<u64>,
    pub prompt: String,
    pub outcome: Option<String>,
    pub error: Option<String>,
}

/// Every agent a run has called, and its phases in the order they began.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Board {
    pub phases: Vec<String>,
    pub agents: Vec<AgentRow>,
}

impl Board {
    /// The phases to list: those the script named, then any an agent used that
    /// the script did not announce, then `Other` when some agents had none.
    pub fn phase_list(&self) -> Vec<String> {
        let mut out = self.phases.clone();
        for agent in &self.agents {
            if let Some(phase) = &agent.phase
                && !out.contains(phase)
            {
                out.push(phase.clone());
            }
        }
        if self.agents.iter().any(|a| a.phase.is_none()) {
            out.push("Other".to_string());
        }
        out
    }

    /// The agents of a phase as [`Board::phase_list`] names it.
    pub fn agents_of(&self, phase: &str) -> Vec<&AgentRow> {
        let unnamed = phase == "Other" && !self.phases.iter().any(|p| p == "Other");
        self.agents
            .iter()
            .filter(|agent| match &agent.phase {
                Some(own) => own == phase,
                None => unnamed,
            })
            .collect()
    }

    /// Agents that have finished, out of all of them.
    pub fn progress(&self) -> (usize, usize) {
        (
            self.agents.iter().filter(|a| a.state.finished()).count(),
            self.agents.len(),
        )
    }

    /// Finished and total agents in a phase.
    pub fn phase_progress(&self, phase: &str) -> (usize, usize) {
        let agents = self.agents_of(phase);
        (
            agents.iter().filter(|a| a.state.finished()).count(),
            agents.len(),
        )
    }
}

/// `text` cut to what a row keeps.
pub fn kept(text: &str) -> String {
    match text.char_indices().nth(KEPT_CHARS) {
        Some((end, _)) => format!("{}...", &text[..end]),
        None => text.to_string(),
    }
}

fn path(run_id: &str) -> Result<PathBuf> {
    Ok(store::run_dir(run_id)?.join("agents.json"))
}

/// Write the board through a temp file so a reader never sees half of it.
pub fn write(run_id: &str, board: &Board) -> Result<()> {
    let target = path(run_id)?;
    let tmp = target.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(board)?)?;
    std::fs::rename(&tmp, target)?;
    Ok(())
}

/// The board a run last wrote, if it wrote one.
pub fn read(run_id: &str) -> Option<Board> {
    let raw = std::fs::read(path(run_id).ok()?).ok()?;
    serde_json::from_slice(&raw).ok()
}

fn stop_path(run_id: &str) -> Result<PathBuf> {
    Ok(store::run_dir(run_id)?.join("stop"))
}

/// Ask the process running `run_id` to stop it, from any process.
pub fn request_stop(run_id: &str) -> Result<()> {
    std::fs::write(stop_path(run_id)?, b"stop\n")?;
    Ok(())
}

pub fn stop_requested(run_id: &str) -> bool {
    stop_path(run_id).is_ok_and(|p| p.exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(i: u32, phase: Option<&str>, state: AgentState) -> AgentRow {
        AgentRow {
            i,
            label: format!("a{i}"),
            phase: phase.map(str::to_string),
            state,
            agent: "worker".into(),
            started_ms: None,
            ended_ms: None,
            prompt: String::new(),
            outcome: None,
            error: None,
        }
    }

    #[test]
    fn phases_list_announced_then_used_then_other() {
        let board = Board {
            phases: vec!["Scan".into(), "Fix".into()],
            agents: vec![
                row(1, Some("Scan"), AgentState::Done),
                row(2, Some("Scan"), AgentState::Running),
                row(3, Some("Surprise"), AgentState::Queued),
                row(4, None, AgentState::Failed),
            ],
        };
        assert_eq!(board.phase_list(), ["Scan", "Fix", "Surprise", "Other"]);
        assert_eq!(board.phase_progress("Scan"), (1, 2));
        assert_eq!(board.phase_progress("Fix"), (0, 0));
        assert_eq!(board.agents_of("Other").len(), 1);
        assert_eq!(board.progress(), (2, 4));
    }

    #[test]
    fn a_long_prompt_is_cut() {
        assert_eq!(kept("short"), "short");
        let long = "x".repeat(KEPT_CHARS + 10);
        assert!(kept(&long).ends_with("..."));
        assert_eq!(kept(&long).chars().count(), KEPT_CHARS + 3);
    }
}
