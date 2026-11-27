//! What a session is doing, in the few words a terminal tab has room for.
//!
//! A backend driver sees both halves of the conversation with its frontend:
//! every [`Request`] going in and every [`Event`] coming out. A [`Tracker`]
//! fed with both knows whether a turn is running, waiting on a permission
//! prompt, or finished, and what the session is about. That is the same for
//! every frontend, so it is worked out once here instead of in each of them.
//!
//! The driver does not write to the terminal. It runs on another task, and
//! bytes written in the middle of a frame would land inside whatever escape
//! sequence the frontend was halfway through. It [`post`]s the status instead,
//! and the frontend's event loop [`take`]s it between frames.

use std::sync::Mutex;

use crate::{Event, HistoryItem, Request, StopReason, ToolStatus};

/// What a program can report about itself. The states of the Program Status
/// Protocol (OSC 7501).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum State {
    /// Nothing has run yet.
    #[default]
    Idle,
    Working,
    /// Waiting on the person at the keyboard.
    Blocked(Kind),
    /// A turn ran and has ended.
    Done,
    Error,
}

/// What a blocked session is waiting for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Permission,
    Question,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub state: State,
    /// One line on what is happening: the running tool, the call that needs
    /// an answer. Empty when there is nothing to add to `state`.
    pub msg: String,
    /// What the session is about: the first thing the user asked for. Empty
    /// until they ask.
    pub title: String,
}

/// Follows one session through its requests and events.
#[derive(Debug, Default)]
pub struct Tracker {
    status: Status,
    /// The prompt most recently sent. It becomes the title if a turn starts
    /// for it; a `/command` the backend answers by itself starts none.
    prompt: String,
}

impl Tracker {
    pub fn status(&self) -> &Status {
        &self.status
    }

    /// Returns whether the status changed.
    pub fn on_request(&mut self, request: &Request) -> bool {
        let before = self.status.clone();
        match request {
            Request::Prompt(text) | Request::Steer(text) | Request::PromptWith { text, .. } => {
                self.prompt = first_line(text);
            }
            Request::NewSession | Request::LoadSession(_) => {
                self.status = Status::default();
                self.prompt.clear();
            }
            Request::Decide { .. } | Request::Answer { .. } => {
                if matches!(self.status.state, State::Blocked(_)) {
                    self.status.state = State::Working;
                    self.status.msg.clear();
                }
            }
            _ => {}
        }
        self.status != before
    }

    /// Returns whether the status changed.
    pub fn on_event(&mut self, event: &Event) -> bool {
        let before = self.status.clone();
        match event {
            Event::TurnStart => {
                self.status.state = State::Working;
                self.status.msg.clear();
                if self.status.title.is_empty() {
                    self.status.title = std::mem::take(&mut self.prompt);
                }
            }
            Event::Tool(call)
                if self.status.state == State::Working
                    && call.parent_id.is_none()
                    && matches!(call.status, ToolStatus::Pending | ToolStatus::Running) =>
            {
                self.status.msg = first_line(or(&call.title, &call.name));
            }
            Event::Permission(request) => {
                let kind = if request.tool == "AskUserQuestion" {
                    Kind::Question
                } else {
                    Kind::Permission
                };
                self.status.state = State::Blocked(kind);
                self.status.msg = first_line(or(&request.title, &request.tool));
            }
            Event::TurnEnd(reason) => {
                self.status.state = match reason {
                    StopReason::Error => State::Error,
                    _ => State::Done,
                };
                self.status.msg.clear();
            }
            Event::Fatal(_) => {
                self.status.state = State::Error;
                self.status.msg.clear();
            }
            Event::History { items, .. } => {
                self.status.title = items
                    .iter()
                    .find_map(|item| match item {
                        HistoryItem::User(text) => Some(first_line(text)),
                        _ => None,
                    })
                    .unwrap_or_default();
            }
            _ => {}
        }
        self.status != before
    }
}

fn or<'a>(first: &'a str, second: &'a str) -> &'a str {
    if first.trim().is_empty() {
        second
    } else {
        first
    }
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").trim().to_string()
}

/// The newest status nobody has shown yet. One slot, because a process runs
/// one session in one terminal, and only the newest is worth showing.
static LATEST: Mutex<Option<Status>> = Mutex::new(None);

/// Leave `status` for the frontend's event loop to show.
pub fn post(status: Status) {
    *LATEST.lock().unwrap_or_else(|e| e.into_inner()) = Some(status);
}

/// The status posted since the last call, if any.
pub fn take() -> Option<Status> {
    LATEST.lock().unwrap_or_else(|e| e.into_inner()).take()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PermissionRequest, ToolCall};

    fn started(prompt: &str) -> Tracker {
        let mut t = Tracker::default();
        t.on_request(&Request::Prompt(prompt.to_string()));
        assert!(t.on_event(&Event::TurnStart));
        t
    }

    #[test]
    fn a_turn_is_working_then_done_and_takes_its_title_from_the_prompt() {
        let mut t = Tracker::default();
        assert_eq!(t.status().state, State::Idle);
        assert!(!t.on_request(&Request::Prompt("fix the parser\nit drops commas".into())));
        assert!(t.on_event(&Event::TurnStart));
        assert_eq!(t.status().state, State::Working);
        assert_eq!(t.status().title, "fix the parser");
        assert!(t.on_event(&Event::TurnEnd(StopReason::EndTurn)));
        assert_eq!(t.status().state, State::Done);

        // The title is the first prompt, not the latest.
        t.on_request(&Request::Prompt("now the tests".into()));
        t.on_event(&Event::TurnStart);
        assert_eq!(t.status().title, "fix the parser");
    }

    #[test]
    fn a_command_that_starts_no_turn_is_not_a_title() {
        let mut t = Tracker::default();
        t.on_request(&Request::Prompt("/model".into()));
        t.on_request(&Request::Prompt("real work".into()));
        t.on_event(&Event::TurnStart);
        assert_eq!(t.status().title, "real work");
    }

    #[test]
    fn a_permission_prompt_blocks_until_it_is_answered() {
        let mut t = started("run the tests");
        t.on_event(&Event::Permission(PermissionRequest {
            id: "1".into(),
            tool: "execute".into(),
            title: "cargo test".into(),
            ..Default::default()
        }));
        assert_eq!(t.status().state, State::Blocked(Kind::Permission));
        assert_eq!(t.status().msg, "cargo test");
        assert!(t.on_request(&Request::Decide {
            id: "1".into(),
            allow: true,
            scope: Default::default(),
            note: String::new(),
        }));
        assert_eq!(t.status().state, State::Working);
        assert_eq!(t.status().msg, "");
    }

    #[test]
    fn a_question_is_not_a_permission() {
        let mut t = started("plan it");
        t.on_event(&Event::Permission(PermissionRequest {
            tool: "AskUserQuestion".into(),
            ..Default::default()
        }));
        assert_eq!(t.status().state, State::Blocked(Kind::Question));
    }

    #[test]
    fn the_running_tool_is_the_message_and_a_subagents_tool_is_not() {
        let mut t = started("look around");
        t.on_event(&Event::Tool(ToolCall {
            name: "execute".into(),
            title: "ls -la".into(),
            status: ToolStatus::Running,
            ..Default::default()
        }));
        assert_eq!(t.status().msg, "ls -la");
        assert!(!t.on_event(&Event::Tool(ToolCall {
            name: "read_file".into(),
            status: ToolStatus::Running,
            parent_id: Some("sub".into()),
            ..Default::default()
        })));
        assert!(!t.on_event(&Event::Tool(ToolCall {
            name: "execute".into(),
            title: "ls -la".into(),
            status: ToolStatus::Completed,
            ..Default::default()
        })));
    }

    #[test]
    fn a_failed_turn_is_an_error() {
        let mut t = started("anything");
        t.on_event(&Event::TurnEnd(StopReason::Error));
        assert_eq!(t.status().state, State::Error);
        let mut t = started("anything");
        t.on_event(&Event::TurnEnd(StopReason::Cancelled));
        assert_eq!(t.status().state, State::Done);
    }

    #[test]
    fn a_resumed_session_is_titled_by_its_first_prompt_and_a_new_one_by_nothing() {
        let mut t = Tracker::default();
        t.on_event(&Event::History {
            session_id: "s".into(),
            items: vec![
                HistoryItem::Assistant("hello".into()),
                HistoryItem::User("port it to zig".into()),
                HistoryItem::User("and benchmark".into()),
            ],
        });
        assert_eq!(t.status().title, "port it to zig");
        assert!(t.on_request(&Request::NewSession));
        assert_eq!(*t.status(), Status::default());
    }
}
