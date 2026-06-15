//! Agent facade.
//!
//! Wizard's agent is the NexAU code agent, reached over the Python bridge
//! ([`crate::backend::nexau`]). This module owns the UI-facing event contract
//! ([`AgentEvent`]) and a thin [`Agent`] that holds the live bridge and
//! forwards each turn to it. The loop itself runs inside NexAU; Wizard only
//! streams and renders its events.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::auth::xai_oauth::XaiTokenSource;
use crate::backend::nexau::{Bridge, BridgeConfig};
use crate::config::Mode;

/// Why an agent turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoneReason {
    /// The agent finished the turn normally.
    Completed,
    /// The turn was interrupted or the bridge stopped it.
    Stopped,
}

/// Output of a single tool call, as surfaced to the UI tool card.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    /// Text the tool returned (shell stdout/stderr, etc.).
    pub content: String,
    /// True when the tool reported a failure.
    pub is_error: bool,
}

/// Events emitted as a turn runs. The TUI renders them.
#[derive(Debug)]
pub enum AgentEvent {
    /// Streaming assistant text delta.
    TextDelta(String),
    /// Streaming model reasoning ("thinking") delta; rendered dimmed and
    /// never kept in the transcript.
    ThinkingDelta(String),
    /// A tool call started (args fully accumulated).
    ToolStarted { name: String, args: Value },
    /// A tool call finished.
    ToolFinished { name: String, output: ToolOutput },
    /// One agent step (a tool round-trip) completed. 1-based.
    StepCompleted { step: u32 },
    /// Non-fatal error surfaced to the user; the turn may continue.
    Error(String),
    /// The turn is over.
    Done { reason: DoneReason },
}

/// Send an event, reporting whether the receiver is still listening.
pub(crate) async fn emit(events: &mpsc::Sender<AgentEvent>, event: AgentEvent) -> bool {
    events.send(event).await.is_ok()
}

/// The agent: a live bridge subprocess plus the config needed to respawn it
/// (model switch, `/clear`).
pub struct Agent {
    bridge: Bridge,
    cfg: BridgeConfig,
    mode: Mode,
    /// xAI OAuth source, when the agent authenticates by sign-in. Consulted
    /// before each turn to keep the bearer token fresh.
    tokens: Option<Arc<XaiTokenSource>>,
}

impl Agent {
    /// Spawn the bridge and wait for it to be ready. `tokens` is `Some` when
    /// the endpoint is reached via xAI OAuth; `cfg.api_key` must then already
    /// hold a freshly fetched bearer.
    pub async fn spawn(
        cfg: BridgeConfig,
        mode: Mode,
        tokens: Option<Arc<XaiTokenSource>>,
    ) -> Result<Self> {
        let bridge = Bridge::spawn(&cfg).await?;
        Ok(Self {
            bridge,
            cfg,
            mode,
            tokens,
        })
    }

    /// Run one user turn, streaming events onto `events`. Refreshes the OAuth
    /// bearer first (when signed in) and pushes it to the bridge if it changed,
    /// so a long session never dies on an expired token.
    pub async fn run_turn(
        &mut self,
        input: &str,
        events: mpsc::Sender<AgentEvent>,
    ) -> Result<DoneReason> {
        if let Some(tokens) = &self.tokens {
            let bearer = tokens.bearer().await?;
            if bearer != self.cfg.api_key {
                self.cfg.api_key = bearer.clone();
                self.bridge.set_api_key(&bearer).await?;
            }
        }
        self.bridge.run_turn(input, events).await
    }

    /// Active model tag.
    pub fn model(&self) -> &str {
        &self.cfg.model
    }

    /// Current personality mode.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Switch personality mode (no agent restart needed).
    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    /// Switch the model: respawns the bridge with the new `LLM_MODEL`, which
    /// also resets conversation history.
    pub async fn set_model(&mut self, model: impl Into<String>) -> Result<()> {
        self.cfg.model = model.into();
        self.respawn().await
    }

    /// Clear the conversation by respawning a fresh bridge.
    pub async fn clear(&mut self) -> Result<()> {
        self.respawn().await
    }

    async fn respawn(&mut self) -> Result<()> {
        let fresh = Bridge::spawn(&self.cfg).await?;
        let old = std::mem::replace(&mut self.bridge, fresh);
        old.shutdown().await;
        Ok(())
    }

    /// Tell the in-flight turn to cancel (best effort).
    pub async fn interrupt(&mut self) {
        self.bridge.interrupt().await;
    }

    /// Shut the bridge down cleanly.
    pub async fn shutdown(self) {
        self.bridge.shutdown().await;
    }
}
