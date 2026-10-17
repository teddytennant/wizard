//! Scripted backend for the comparison harness: plays a file of events back so the look can be
//! put through the same turn the reference captures show, with no model call.
//!
//! Selected with `CLAUDEW_BACKEND=replay` and `CLAUDEW_REPLAY=<file>`. One JSON object per line:
//!
//! - `{"event": <agent_core::Event>}` sends the event
//! - `{"await": "prompt"}` waits for the next prompt, `{"await": "decide"}` for a permission answer
//! - `{"sleep": 300}` waits that many milliseconds
//! - `{"sessions": [<SessionInfo>, ...]}` sets the answer to `ListSessions`; an `updated` of
//!   zero or less is that many seconds before the list is asked for
//!
//! `/plan` is answered on the spot, as wizard answers it. Model, effort and mode changes are applied to the config from the last `Ready` and echoed as
//! `ConfigChanged`. `Cancel` ends the turn and skips to the next `await`.

use std::path::Path;
use std::time::Duration;

use agent_core::{BackendHandle, Config, Event, Request, SessionInfo, StopReason};
use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
enum Step {
    Event(Event),
    Await(String),
    Sleep(u64),
    Sessions(Vec<SessionInfo>),
}

#[derive(PartialEq)]
enum Got {
    Prompt,
    Decide,
    Cancel,
    Other,
    Quit,
}

struct Player {
    tx: UnboundedSender<Event>,
    config: Config,
    sessions: Vec<SessionInfo>,
    session_id: String,
    plan: bool,
}

impl Player {
    fn handle(&mut self, req: Request) -> Got {
        match req {
            Request::Prompt(p) if p.trim() == "/plan" => {
                self.plan = !self.plan;
                let state = if self.plan { "on" } else { "off" };
                let _ = self.tx.send(Event::TurnStart);
                let _ = self.tx.send(Event::Notice {
                    level: agent_core::NoticeLevel::Info,
                    text: format!("plan mode {state}"),
                });
                let _ = self.tx.send(Event::TurnEnd(StopReason::EndTurn));
                Got::Other
            }
            Request::Prompt(_) | Request::PromptWith { .. } | Request::Steer(_) => Got::Prompt,
            Request::Decide { allow: false, .. } => Got::Cancel,
            Request::Decide { .. } | Request::Answer { .. } => Got::Decide,
            Request::Cancel => Got::Cancel,
            Request::Shutdown => Got::Quit,
            Request::ListSessions => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs() as i64);
                let mut list = self.sessions.clone();
                for x in &mut list {
                    if x.updated <= 0 {
                        x.updated += now;
                    }
                }
                let _ = self.tx.send(Event::Sessions(list));
                Got::Other
            }
            Request::NewSession | Request::LoadSession(_) => {
                let _ = self.tx.send(Event::Ready {
                    session_id: self.session_id.clone(),
                    config: self.config.clone(),
                });
                Got::Other
            }
            Request::SetModel(m) => {
                self.config.model = m;
                self.changed()
            }
            Request::SetEffort(e) => {
                self.config.effort = e;
                self.changed()
            }
            Request::SetMode(m) => {
                self.config.mode = m;
                self.changed()
            }
            _ => Got::Other,
        }
    }

    fn changed(&self) -> Got {
        let _ = self.tx.send(Event::ConfigChanged(self.config.clone()));
        Got::Other
    }
}

pub fn spawn(path: &Path) -> Result<BackendHandle> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("could not read the replay script {}", path.display()))?;
    let mut steps = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let step: Step = serde_json::from_str(line)
            .with_context(|| format!("{}:{}: not a replay step", path.display(), i + 1))?;
        steps.push(step);
    }
    let (req_tx, req_rx) = unbounded_channel();
    let (ev_tx, ev_rx) = unbounded_channel();
    agent_core::spawn_supervised("replay", ev_tx.clone(), play(steps, req_rx, ev_tx));
    Ok(BackendHandle {
        tx: req_tx,
        rx: ev_rx,
    })
}

async fn play(steps: Vec<Step>, mut rx: UnboundedReceiver<Request>, tx: UnboundedSender<Event>) {
    let mut p = Player {
        tx,
        config: Config::default(),
        sessions: Vec::new(),
        session_id: String::new(),
        plan: false,
    };
    let mut prompts = 0usize;
    let mut skipping = false;
    for step in steps {
        match step {
            Step::Await(what) => {
                skipping = false;
                if what == "prompt" && prompts > 0 {
                    prompts -= 1;
                    continue;
                }
                loop {
                    let Some(req) = rx.recv().await else { return };
                    match p.handle(req) {
                        Got::Quit => return,
                        Got::Prompt if what == "prompt" => break,
                        Got::Decide if what == "decide" => break,
                        Got::Cancel if what != "prompt" => {
                            let _ = p.tx.send(Event::TurnEnd(StopReason::Cancelled));
                            skipping = true;
                            break;
                        }
                        _ => {}
                    }
                }
            }
            _ if skipping => {}
            Step::Sessions(s) => p.sessions = s,
            Step::Event(ev) => {
                if let Event::Ready { session_id, config } = &ev {
                    p.session_id = session_id.clone();
                    p.config = config.clone();
                }
                let _ = p.tx.send(ev);
            }
            Step::Sleep(ms) => {
                let sleep = tokio::time::sleep(Duration::from_millis(ms));
                tokio::pin!(sleep);
                loop {
                    tokio::select! {
                        _ = &mut sleep => break,
                        req = rx.recv() => {
                            let Some(req) = req else { return };
                            match p.handle(req) {
                                Got::Quit => return,
                                Got::Prompt => prompts += 1,
                                Got::Cancel => {
                                    let _ = p.tx.send(Event::TurnEnd(StopReason::Cancelled));
                                    skipping = true;
                                    break;
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
    }
    while let Some(req) = rx.recv().await {
        if p.handle(req) == Got::Quit {
            return;
        }
    }
}
