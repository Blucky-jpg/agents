//! Cross-task messages: UI → agent and agent → UI.
//!
//! The UI thread and the agent task never share `Runner` (the agent task owns
//! it). They communicate through two `tokio::sync::mpsc` channels:
//!
//! - `tx_ui_to_agent: mpsc::UnboundedSender<UiToAgent>` — UI sends the user's
//!   prompt or a shutdown signal.
//! - `tx_agent_to_ui: mpsc::UnboundedSender<AgentToUi>` — agent reports turn
//!   completion (success or error).

use co_scientist::bus::MemoryEvent;
use co_scientist::skill::Marker;
use tokio::sync::watch;

/// UI → agent.
#[derive(Debug)]
pub enum UiToAgent {
    Turn {
        agent_name: String,
        user_text: String,
    },
    StartSupervisor { goal: String },
    Shutdown,
}

/// Agent → UI.
#[derive(Debug)]
pub enum AgentToUi {
    /// A single-agent turn has begun. Carries the model the runner is
    /// actually using so the UI displays the truth rather than reading
    /// `CO_SCIENTIST_MODEL` independently (and possibly disagreeing).
    TurnStarted { model: String },
    /// A chunk of raw (uncleaned) assistant text just arrived from the
    /// LLM stream. The UI appends this to the in-flight assistant
    /// message so the user sees the response token-by-token.
    TurnDelta {
        agent_name: String,
        delta: String,
    },
    TurnDone {
        cleaned_text: String,
        markers: Vec<Marker>,
        agent_name: String,
    },
    TurnFailed {
        error: String,
        agent_name: String,
    },
    /// A supervisor session has been spawned. The `stop_tx` lets the UI send
    /// `/stop` to flip the supervisor's shutdown channel.
    SupervisorStarted {
        session_id: String,
        stop_tx: watch::Sender<bool>,
    },
    /// A live event from the `MemoryEvent` bus (TaskClaimed, SemanticSaved…).
    SupervisorEvent(MemoryEvent),
    /// Supervisor returned; session finished with `reason`.
    SupervisorFinished { reason: String, session_id: String },
    /// Supervisor failed to start (e.g. claude CLI missing).
    SupervisorFailed { error: String },
}