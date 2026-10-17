//! Folds the [`Event`] stream into a transcript both frontends render.
//!
//! Rendering stays in the frontends; this only decides what a message is made
//! of. Deltas extend the last part of the same kind, a repeated tool id
//! replaces the earlier call in place, and a replayed [`Event::History`] rebuilds
//! the list from scratch.

use std::time::{Duration, Instant};

use crate::{Event, HistoryItem, NoticeLevel, StopReason, Todo, ToolCall, Usage};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    /// Slash-command output and other out-of-band text.
    Notice(NoticeLevel),
}

#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Part {
    Text(String),
    Thought {
        text: String,
        started: Instant,
        took: Option<Duration>,
    },
    Tool(ToolCall),
}

#[derive(Clone, Debug)]
pub struct Message {
    pub role: Role,
    pub parts: Vec<Part>,
    pub started: Instant,
    /// Set when the assistant turn ended.
    pub took: Option<Duration>,
    pub stop: Option<StopReason>,
    /// Model name at the time the turn started (assistant only).
    pub model: String,
}

impl Message {
    fn new(role: Role, model: &str) -> Self {
        Message {
            role,
            parts: Vec::new(),
            started: Instant::now(),
            took: None,
            stop: None,
            model: model.to_string(),
        }
    }
}

#[derive(Debug, Default)]
pub struct Transcript {
    pub messages: Vec<Message>,
    pub todos: Vec<Todo>,
    pub usage: Usage,
    pub busy: bool,
    pub session_id: String,
    pub model: String,
    /// Bumps on every change so a renderer can cache cheaply.
    pub rev: u64,
}

impl Transcript {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_user(&mut self, text: &str) {
        let mut m = Message::new(Role::User, "");
        m.parts.push(Part::Text(text.to_string()));
        self.messages.push(m);
        self.rev += 1;
    }

    fn assistant(&mut self) -> &mut Message {
        // The open turn keeps streaming into its own message even when the user queued another
        // prompt behind it, which sits after it in the list.
        // Only user prompts and notices can follow the open message (a queued prompt waits for
        // the turn to end), so the first assistant message from the end is the only candidate;
        // scanning the whole list made a replayed history quadratic.
        let open = self
            .messages
            .iter()
            .rposition(|m| m.role == Role::Assistant)
            .filter(|&i| self.messages[i].took.is_none());
        let at = match open {
            Some(i) => i,
            None => {
                self.messages
                    .push(Message::new(Role::Assistant, &self.model));
                self.messages.len() - 1
            }
        };
        &mut self.messages[at]
    }

    /// Close an open thought so its duration is fixed when something else arrives.
    fn close_thought(m: &mut Message) {
        if let Some(Part::Thought { took, started, .. }) = m.parts.last_mut() {
            if took.is_none() {
                *took = Some(started.elapsed());
            }
        }
    }

    pub fn apply(&mut self, ev: &Event) {
        self.rev += 1;
        match ev {
            Event::Ready { session_id, config } => {
                self.session_id = session_id.clone();
                self.model = config.model.clone();
            }
            Event::ConfigChanged(c) => self.model = c.model.clone(),
            Event::History { session_id, items } => {
                self.session_id = session_id.clone();
                self.messages.clear();
                self.todos.clear();
                self.busy = false;
                for it in items {
                    self.replay(it);
                }
                self.close_replayed();
            }
            Event::TurnStart => {
                self.busy = true;
                let m = self.assistant();
                m.started = Instant::now();
            }
            Event::TextDelta(t) => {
                self.busy = true;
                let m = self.assistant();
                Self::close_thought(m);
                match m.parts.last_mut() {
                    Some(Part::Text(s)) => s.push_str(t),
                    _ => m.parts.push(Part::Text(t.clone())),
                }
            }
            Event::ThoughtDelta(t) => {
                self.busy = true;
                let m = self.assistant();
                match m.parts.last_mut() {
                    Some(Part::Thought {
                        text, took: None, ..
                    }) => text.push_str(t),
                    _ => m.parts.push(Part::Thought {
                        text: t.clone(),
                        started: Instant::now(),
                        took: None,
                    }),
                }
            }
            Event::Tool(call) => {
                self.busy = true;
                let m = self.assistant();
                Self::close_thought(m);
                let slot = m.parts.iter_mut().find_map(|p| match p {
                    Part::Tool(c) if c.id == call.id => Some(c),
                    _ => None,
                });
                match slot {
                    Some(c) => *c = call.clone(),
                    None => m.parts.push(Part::Tool(call.clone())),
                }
            }
            Event::Todos(t) => self.todos = t.clone(),
            Event::Usage(u) => self.usage = u.clone(),
            Event::TurnEnd(reason) => {
                self.busy = false;
                // A slash command's notice can land after the assistant message its turn opened,
                // so close the newest open one rather than only the last message.
                if let Some(m) = self
                    .messages
                    .iter_mut()
                    .rev()
                    .find(|m| m.role == Role::Assistant && m.took.is_none())
                {
                    Self::close_thought(m);
                    m.took = Some(m.started.elapsed());
                    m.stop = Some(*reason);
                }
            }
            Event::Notice { level, text } => {
                let mut m = Message::new(Role::Notice(*level), "");
                m.parts.push(Part::Text(text.clone()));
                self.messages.push(m);
            }
            Event::Fatal(text) => {
                self.busy = false;
                let mut m = Message::new(Role::Notice(NoticeLevel::Error), "");
                m.parts.push(Part::Text(text.clone()));
                self.messages.push(m);
            }
            Event::Commands(_)
            | Event::Sessions(_)
            | Event::Permission(_)
            | Event::RewindPreview(_) => {}
        }
    }

    fn replay(&mut self, it: &HistoryItem) {
        match it {
            HistoryItem::User(t) => {
                self.close_replayed();
                self.push_user(t);
            }
            HistoryItem::Assistant(t) => self.apply_quiet(Event::TextDelta(t.clone())),
            HistoryItem::Thought(t) => self.apply_quiet(Event::ThoughtDelta(t.clone())),
            HistoryItem::Tool(c) => self.apply_quiet(Event::Tool(c.clone())),
        }
    }

    /// A replayed answer is finished: the next turn starts a message of its own.
    fn close_replayed(&mut self) {
        if let Some(m) = self.messages.last_mut() {
            if m.role == Role::Assistant && m.took.is_none() {
                m.took = Some(Duration::ZERO);
            }
        }
    }

    fn apply_quiet(&mut self, ev: Event) {
        self.apply(&ev);
        self.busy = false;
        // Replayed thoughts have no real duration; do not claim one.
        // a delta only ever touches the last part; earlier thoughts were closed when it began
        if let Some(Part::Thought { took, .. }) =
            self.messages.last_mut().and_then(|m| m.parts.last_mut())
        {
            took.get_or_insert(Duration::ZERO);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, ToolStatus};

    fn call(id: &str, status: ToolStatus) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "execute".into(),
            status,
            ..Default::default()
        }
    }

    #[test]
    fn deltas_extend_and_turn_closes() {
        let mut t = Transcript::new();
        t.push_user("hi");
        t.apply(&Event::TurnStart);
        t.apply(&Event::TextDelta("he".into()));
        t.apply(&Event::TextDelta("llo".into()));
        assert!(t.busy);
        t.apply(&Event::TurnEnd(StopReason::EndTurn));
        assert!(!t.busy);
        assert_eq!(t.messages.len(), 2);
        let m = &t.messages[1];
        assert!(matches!(&m.parts[..], [Part::Text(s)] if s == "hello"));
        assert!(m.took.is_some());
    }

    #[test]
    fn a_notice_between_start_and_end_does_not_leave_the_turn_open() {
        let mut t = Transcript::new();
        t.push_user("/compact");
        t.apply(&Event::TurnStart);
        t.apply(&Event::Notice {
            level: crate::NoticeLevel::Info,
            text: "nothing to compact yet".into(),
        });
        t.apply(&Event::TurnEnd(StopReason::EndTurn));
        assert!(t
            .messages
            .iter()
            .all(|m| m.role != Role::Assistant || m.took.is_some()));
        assert!(!t.busy);
    }

    #[test]
    fn a_queued_prompt_does_not_split_the_running_turn() {
        let mut t = Transcript::new();
        t.push_user("first");
        t.apply(&Event::TurnStart);
        t.apply(&Event::ThoughtDelta("hmm".into()));
        t.push_user("second");
        t.apply(&Event::TextDelta("answer".into()));
        t.apply(&Event::TurnEnd(StopReason::EndTurn));
        // first user, its assistant message (thought and text), then the queued user
        assert_eq!(t.messages.len(), 3);
        assert!(
            matches!(&t.messages[1].parts[..], [Part::Thought { took: Some(_), .. }, Part::Text(s)] if s == "answer")
        );
        assert_eq!(t.messages[2].role, Role::User);
        // the next turn opens a fresh message after the queued prompt
        t.apply(&Event::TurnStart);
        t.apply(&Event::TextDelta("second answer".into()));
        assert_eq!(t.messages.len(), 4);
        assert_eq!(t.messages[3].role, Role::Assistant);
    }

    #[test]
    fn tool_update_replaces_in_place() {
        let mut t = Transcript::new();
        t.apply(&Event::Tool(call("a", ToolStatus::Running)));
        t.apply(&Event::TextDelta("x".into()));
        t.apply(&Event::Tool(call("a", ToolStatus::Completed)));
        let m = &t.messages[0];
        assert_eq!(m.parts.len(), 2);
        assert!(matches!(&m.parts[0], Part::Tool(c) if c.status == ToolStatus::Completed));
    }

    #[test]
    fn thought_gets_a_duration_once_text_follows() {
        let mut t = Transcript::new();
        t.apply(&Event::ThoughtDelta("hmm".into()));
        assert!(matches!(
            &t.messages[0].parts[0],
            Part::Thought { took: None, .. }
        ));
        t.apply(&Event::TextDelta("ok".into()));
        assert!(matches!(
            &t.messages[0].parts[0],
            Part::Thought { took: Some(_), .. }
        ));
    }

    #[test]
    fn history_replaces_everything_and_splits_turns() {
        let mut t = Transcript::new();
        t.push_user("old");
        t.apply(&Event::History {
            session_id: "s".into(),
            items: vec![
                HistoryItem::User("q1".into()),
                HistoryItem::Assistant("a1".into()),
                HistoryItem::User("q2".into()),
                HistoryItem::Assistant("a2".into()),
            ],
        });
        assert_eq!(t.messages.len(), 4);
        assert_eq!(t.session_id, "s");
        assert!(!t.busy);
    }

    #[test]
    fn ready_records_model() {
        let mut t = Transcript::new();
        t.apply(&Event::Ready {
            session_id: "s".into(),
            config: Config {
                model: "m".into(),
                ..Default::default()
            },
        });
        t.apply(&Event::TurnStart);
        assert_eq!(t.messages[0].model, "m");
    }

    /// Finding 10: every assistant item of a replay scanned the whole list for an open message,
    /// 15.8 s at 80,000 turns.
    #[test]
    fn a_turn_after_a_replayed_answer_gets_its_own_message() {
        let mut t = Transcript::new();
        t.apply(&Event::History {
            session_id: "s".into(),
            items: vec![
                HistoryItem::User("q".into()),
                HistoryItem::Assistant("old".into()),
            ],
        });
        t.push_user("next");
        t.apply(&Event::TurnStart);
        t.apply(&Event::TextDelta("new".into()));
        assert_eq!(t.messages.len(), 4);
        assert!(matches!(&t.messages[1].parts[..], [Part::Text(s)] if s == "old"));
        assert!(matches!(&t.messages[3].parts[..], [Part::Text(s)] if s == "new"));
    }

    #[test]
    fn replaying_a_long_history_is_linear() {
        let replay = |n: usize| {
            let items: Vec<HistoryItem> = (0..n)
                .flat_map(|i| {
                    [
                        HistoryItem::User(format!("q{i}")),
                        HistoryItem::Thought("hm".into()),
                        HistoryItem::Assistant(format!("a{i}")),
                    ]
                })
                .collect();
            let mut t = Transcript::new();
            let start = Instant::now();
            t.apply(&Event::History {
                session_id: "s".into(),
                items,
            });
            assert_eq!(t.messages.len(), 2 * n);
            start.elapsed().as_secs_f64()
        };
        let small = replay(10_000);
        let big = replay(40_000);
        // 4x the input costs about 4x; quadratic costs about 16x
        assert!(
            big < small * 9.0 + 0.1,
            "10k turns {small:.3}s, 40k turns {big:.3}s"
        );
    }
}
