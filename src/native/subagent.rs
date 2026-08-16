//! The subagent rail: one model per concurrent run.
//!
//! # Why this cannot live in the transcript
//!
//! [`TranscriptModel::apply`] drops every `SubagentRun*` event on purpose. It is
//! the *conversation*, and a subagent's tool calls are not part of it: folding
//! them in would interleave four concurrent runs' output into one stream where
//! no reader could tell whose `read_file` was whose, and the parent's own
//! narration would be lost in it. The browser GUI reached the same conclusion
//! and demuxed by `run` id in JavaScript.
//!
//! So a run gets its own [`TranscriptModel`], and the events are translated
//! into the shapes that model already folds — `SubagentRunToolStarted` *is* a
//! `ToolStarted`, once you know whose it is. That translation is the whole
//! module, and it is what lets a run's pane be drawn by the same
//! [`crate::native::widget::transcript::blocks`] the chat is drawn by, with the
//! same tool rows, the same clipping and the same selection layer over it.
//!
//! # A run outlives the turn that spawned it
//!
//! A backgrounded subagent keeps streaming after `Done`. Runs are therefore
//! kept until they report, not cleared at turn boundaries, and
//! [`Rail::runs`] is ordered oldest-first (the rail draws it the other way up,
//! which is a rendering decision and stays in the view).

use std::time::Instant;

use crate::agent::AgentEvent;
use crate::transcript::TranscriptModel;

/// How a run ended, or that it has not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Running,
    Done,
    /// It ran out of steps. Distinct from a failure: nothing broke, the budget
    /// simply ran out, and the difference decides whether re-running it is
    /// worth anything.
    Budget,
    Failed(String),
}

impl Status {
    /// The word the rail and the pane header show.
    pub fn label(&self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Done => "done",
            Status::Budget => "step limit",
            Status::Failed(_) => "failed",
        }
    }
}

/// One subagent run.
pub struct Run {
    pub id: u64,
    pub name: String,
    /// What it was handed, for the row's fallback line and the pane's subtitle.
    pub task: String,
    pub steps: u32,
    pub status: Status,
    /// Its own conversation, folded from the translated events.
    pub transcript: TranscriptModel,
    /// Entries appended while this run's pane was not open.
    pub unread: usize,
    started: Instant,
    finished: Option<Instant>,
}

impl Run {
    /// How long it has been going, or how long it went.
    pub fn elapsed(&self) -> std::time::Duration {
        self.finished.unwrap_or_else(Instant::now) - self.started
    }

    /// The one-line "what it is doing right now" the rail row shows.
    ///
    /// The newest still-open tool call while it is working, because that is the
    /// answer to the question a person is actually asking; its last words when
    /// there is no call in flight; and the task it was handed when it has not
    /// said anything yet. Never blank: a row with an empty middle line reads as
    /// a run that has stalled.
    pub fn activity(&self) -> String {
        use crate::transcript::TranscriptItem;
        let items = self.transcript.items();
        let open = items.iter().rev().find_map(|item| match item {
            TranscriptItem::Tool(tool) if tool.output.is_none() => Some(format!(
                "{} {}",
                tool.name,
                crate::transcript::summarize_tool(&tool.name, &tool.args, "")
            )),
            _ => None,
        });
        if let Some(open) = open {
            return open.trim().to_string();
        }
        let said = items.iter().rev().find_map(|item| match item {
            TranscriptItem::Text(text) if !text.trim().is_empty() => Some(text.clone()),
            _ => None,
        });
        let line = said.unwrap_or_else(|| self.task.clone());
        line.lines().next().unwrap_or("").trim().to_string()
    }
}

/// Every run this chat has spawned, oldest first.
#[derive(Default)]
pub struct Rail {
    pub runs: Vec<Run>,
    /// The run whose pane is open, if any. Its entries do not count as unread.
    pub open: Option<u64>,
}

impl Rail {
    /// Fold one event in. Returns whether anything changed, so a view that is
    /// otherwise idle does not rebuild for the ninety per cent of events that
    /// belong to the parent.
    pub fn apply(&mut self, event: &AgentEvent) -> bool {
        match event {
            AgentEvent::SubagentRunStarted {
                run, name, task, ..
            } => {
                let now = Instant::now();
                self.runs.push(Run {
                    id: *run,
                    name: name.clone(),
                    task: task.clone(),
                    steps: 0,
                    status: Status::Running,
                    transcript: TranscriptModel::new(),
                    unread: 0,
                    started: now,
                    finished: None,
                });
                true
            }
            AgentEvent::SubagentRunText { run, text } => self.fold(*run, |model| {
                // A whole message, not a delta: a subagent's narration arrives
                // complete because the parent only hears about it once the
                // subagent's own step ended.
                model.assistant(text.clone());
            }),
            AgentEvent::SubagentRunToolStarted { run, name, args } => self.fold(*run, |model| {
                model.apply(&AgentEvent::ToolStarted {
                    name: name.clone(),
                    args: args.clone(),
                });
            }),
            AgentEvent::SubagentRunToolFinished { run, name, output } => self.fold(*run, |model| {
                model.apply(&AgentEvent::ToolFinished {
                    name: name.clone(),
                    output: output.clone(),
                });
            }),
            AgentEvent::SubagentRunImages {
                run,
                source,
                images,
            } => self.fold(*run, |model| {
                model.apply(&AgentEvent::Images {
                    source: source.clone(),
                    images: images.clone(),
                });
            }),
            AgentEvent::SubagentRunStep { run, step } => {
                let Some(run) = self.run_mut(*run) else {
                    return false;
                };
                run.steps = *step;
                true
            }
            AgentEvent::SubagentRunDone {
                run,
                completed,
                output,
                steps_used,
                error,
            } => {
                let open = self.open;
                let Some(run) = self.run_mut(*run) else {
                    return false;
                };
                run.steps = *steps_used;
                run.finished = Some(Instant::now());
                run.status = match (error, completed) {
                    (Some(error), _) => Status::Failed(error.clone()),
                    (None, true) => Status::Done,
                    (None, false) => Status::Budget,
                };
                // The report, unless the run's last message already was it —
                // a subagent that finished by writing its answer would
                // otherwise say it twice.
                let duplicate = matches!(
                    run.transcript.items().last(),
                    Some(crate::transcript::TranscriptItem::Text(last)) if last.trim() == output.trim()
                );
                if !output.trim().is_empty() && !duplicate {
                    run.transcript.assistant(output.clone());
                }
                if let Some(error) = error {
                    run.transcript.notice(format!("failed: {error}"));
                } else if !completed {
                    run.transcript.notice("hit its step budget".to_string());
                }
                if open != Some(run.id) {
                    run.unread += 1;
                }
                true
            }
            _ => false,
        }
    }

    /// Open a run's pane, which is also what marks it read.
    pub fn open(&mut self, id: u64) {
        self.open = Some(id);
        if let Some(run) = self.run_mut(id) {
            run.unread = 0;
        }
    }

    pub fn close(&mut self) {
        self.open = None;
    }

    /// Take a finished run off the rail. A still-running one is left alone.
    pub fn dismiss(&mut self, id: u64) -> bool {
        let Some(index) = self.runs.iter().position(|run| run.id == id) else {
            return false;
        };
        if matches!(self.runs[index].status, Status::Running) {
            return false;
        }
        if self.open == Some(id) {
            self.open = None;
        }
        self.runs.remove(index);
        true
    }

    pub fn run(&self, id: u64) -> Option<&Run> {
        self.runs.iter().find(|run| run.id == id)
    }

    fn run_mut(&mut self, id: u64) -> Option<&mut Run> {
        self.runs.iter_mut().find(|run| run.id == id)
    }

    /// Apply `fold` to a run's transcript and count the entry as unread when
    /// its pane is not the one on screen.
    fn fold(&mut self, id: u64, fold: impl FnOnce(&mut TranscriptModel)) -> bool {
        let open = self.open;
        let Some(run) = self.run_mut(id) else {
            // A run this window never saw start — it began before the window
            // opened, or before it switched to this chat. Dropped rather than
            // synthesized: a row with a name nobody knows and no task text is
            // worse than no row.
            return false;
        };
        fold(&mut run.transcript);
        run.transcript.commit();
        if open != Some(id) {
            run.unread += 1;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::ImageSource;
    use crate::tools::ToolOutput;
    use crate::transcript::TranscriptItem;
    use serde_json::json;

    fn started(run: u64, name: &str) -> AgentEvent {
        AgentEvent::SubagentRunStarted {
            run,
            bg: None,
            name: name.to_string(),
            task: format!("do {name}'s work"),
        }
    }

    /// The demux, which is the reason this module exists: two runs going at
    /// once keep their tool calls apart. A single shared model would interleave
    /// them and neither pane would be readable.
    #[test]
    fn two_concurrent_runs_keep_their_own_transcripts() {
        let mut rail = Rail::default();
        rail.apply(&started(1, "scout"));
        rail.apply(&started(2, "builder"));
        rail.apply(&AgentEvent::SubagentRunToolStarted {
            run: 1,
            name: "read_file".to_string(),
            args: json!({ "path": "a.rs" }),
        });
        rail.apply(&AgentEvent::SubagentRunToolStarted {
            run: 2,
            name: "execute".to_string(),
            args: json!({ "command": "cargo build" }),
        });
        rail.apply(&AgentEvent::SubagentRunToolFinished {
            run: 2,
            name: "execute".to_string(),
            output: ToolOutput::ok("built"),
        });

        let scout = rail.run(1).expect("run 1");
        let builder = rail.run(2).expect("run 2");
        assert_eq!(scout.transcript.items().len(), 1);
        assert_eq!(builder.transcript.items().len(), 1);
        assert!(matches!(
            &scout.transcript.items()[0],
            TranscriptItem::Tool(tool) if tool.name == "read_file" && tool.output.is_none()
        ));
        assert!(matches!(
            &builder.transcript.items()[0],
            TranscriptItem::Tool(tool) if tool.name == "execute" && tool.output.is_some()
        ));
    }

    /// The rail's middle line answers "what is it doing", and while a call is
    /// open that is the call — not the last thing it said three steps ago.
    #[test]
    fn the_activity_line_prefers_the_open_call_then_the_last_words() {
        let mut rail = Rail::default();
        rail.apply(&started(1, "scout"));
        assert_eq!(rail.run(1).expect("run").activity(), "do scout's work");

        rail.apply(&AgentEvent::SubagentRunText {
            run: 1,
            text: "looking at the lock file\nsecond line".to_string(),
        });
        assert_eq!(
            rail.run(1).expect("run").activity(),
            "looking at the lock file"
        );

        rail.apply(&AgentEvent::SubagentRunToolStarted {
            run: 1,
            name: "execute".to_string(),
            args: json!({ "command": "ls -la" }),
        });
        assert_eq!(rail.run(1).expect("run").activity(), "execute ls -la");
    }

    /// Unread counting is what makes the badge mean anything: entries arriving
    /// into the pane you are looking at are not unread, and opening a pane
    /// clears what accumulated.
    #[test]
    fn entries_count_as_unread_only_while_the_pane_is_closed() {
        let mut rail = Rail::default();
        rail.apply(&started(1, "scout"));
        rail.apply(&AgentEvent::SubagentRunText {
            run: 1,
            text: "one".to_string(),
        });
        assert_eq!(rail.run(1).expect("run").unread, 1);

        rail.open(1);
        assert_eq!(rail.run(1).expect("run").unread, 0);
        rail.apply(&AgentEvent::SubagentRunText {
            run: 1,
            text: "two".to_string(),
        });
        assert_eq!(rail.run(1).expect("run").unread, 0, "watched, so read");

        rail.close();
        rail.apply(&AgentEvent::SubagentRunText {
            run: 1,
            text: "three".to_string(),
        });
        assert_eq!(rail.run(1).expect("run").unread, 1);
    }

    /// A step-limited run is not a failed one, and the rail has to say which:
    /// re-running the first is worth something and re-running the second is
    /// not.
    #[test]
    fn a_budget_stop_and_a_failure_are_different_states() {
        let mut rail = Rail::default();
        rail.apply(&started(1, "scout"));
        rail.apply(&started(2, "builder"));
        rail.apply(&AgentEvent::SubagentRunDone {
            run: 1,
            completed: false,
            output: String::new(),
            steps_used: 12,
            error: None,
        });
        rail.apply(&AgentEvent::SubagentRunDone {
            run: 2,
            completed: false,
            output: String::new(),
            steps_used: 3,
            error: Some("provider refused".to_string()),
        });
        assert_eq!(rail.run(1).expect("run").status, Status::Budget);
        assert_eq!(
            rail.run(2).expect("run").status,
            Status::Failed("provider refused".to_string())
        );
        assert_eq!(rail.run(1).expect("run").steps, 12);
    }

    /// A run whose last message *is* its report must not print it twice. This
    /// is the ordinary case for a subagent that answers in prose.
    #[test]
    fn a_report_that_repeats_the_last_message_is_not_appended_again() {
        let mut rail = Rail::default();
        rail.apply(&started(1, "scout"));
        rail.apply(&AgentEvent::SubagentRunText {
            run: 1,
            text: "the lock file is stale".to_string(),
        });
        rail.apply(&AgentEvent::SubagentRunDone {
            run: 1,
            completed: true,
            output: "the lock file is stale".to_string(),
            steps_used: 2,
            error: None,
        });
        let texts: Vec<&TranscriptItem> = rail
            .run(1)
            .expect("run")
            .transcript
            .items()
            .iter()
            .collect();
        assert_eq!(texts.len(), 1, "{texts:?}");
    }

    #[test]
    fn dismiss_drops_a_finished_run_and_leaves_a_live_one() {
        let mut rail = Rail::default();
        rail.apply(&started(1, "scout"));
        rail.apply(&started(2, "builder"));
        rail.apply(&AgentEvent::SubagentRunDone {
            run: 1,
            completed: true,
            output: "ok".to_string(),
            steps_used: 1,
            error: None,
        });
        assert!(!rail.dismiss(2), "a live run stays");
        assert_eq!(rail.runs.len(), 2);
        assert!(rail.dismiss(1));
        assert_eq!(rail.runs.len(), 1);
        assert_eq!(rail.runs[0].name, "builder");
    }

    /// An event for a run this window never saw start is dropped rather than
    /// inventing a nameless row for it. It happens on every chat switch: the
    /// tap carries no backlog.
    #[test]
    fn events_for_an_unknown_run_are_dropped() {
        let mut rail = Rail::default();
        assert!(!rail.apply(&AgentEvent::SubagentRunText {
            run: 99,
            text: "orphan".to_string(),
        }));
        assert!(rail.runs.is_empty());
    }

    /// A subagent's images belong to its pane, not the parent's chat.
    #[test]
    fn a_runs_images_land_in_its_own_transcript() {
        let mut rail = Rail::default();
        rail.apply(&started(1, "painter"));
        rail.apply(&AgentEvent::SubagentRunImages {
            run: 1,
            source: ImageSource::Tool("render".to_string()),
            images: vec![crate::images::ImageRef {
                path: "/img/a.png".into(),
                mime: "image/png".to_string(),
                bytes: 3,
            }],
        });
        assert!(matches!(
            rail.run(1).expect("run").transcript.items().last(),
            Some(TranscriptItem::Images { .. })
        ));
    }
}
