//! The TUI's agent task — owns the `Runner` and drives single-agent
//! turns on behalf of the UI thread.
//!
//! ## What this module is
//!
//! The TUI is two tasks: the UI thread (event loop + render) and the
//! agent task (this). They communicate through a `tokio::sync::mpsc`
//! pair (`UiToAgent` / `AgentToUi`). The agent task owns the
//! `co_scientist::Runner` and runs user-initiated turns against the
//! active agent. Supervisor sessions are spawned separately via
//! `crate::supervisor_session::start` (the `StartSupervisor` arm
//! just delegates there).
//!
//! ## What this module is not
//!
//! - Not a generic "runner wrapper" — the Runner already has a
//!   streaming API (`turn_stream`). This module is the TUI-specific
//!   glue: forwarder, agent lookup, Runner rebuild after shutdown.
//! - Not concerned with the chat log or `AppState` rendering. It
//!   sends `AgentToUi` events; the UI thread's
//!   `app::reducers::reduce` is the sole consumer.
//!
//! ## Streaming-delta happens-before
//!
//! The `Turn` arm spawns a forwarder task that drains a per-turn
//! `mpsc::UnboundedReceiver<String>` and forwards each delta as
//! `AgentToUi::TurnDelta`. `runner.turn_stream` writes deltas to
//! the same channel; when it returns, `delta_tx` is dropped, the
//! forwarder's `recv()` returns `None`, and the forwarder exits.
//! The `tokio::join!` then completes with the turn's `Result`,
//! guaranteeing all deltas are flushed before we send `TurnDone`.
//! This is the happens-before contract DK14 records for the CLI
//! side; the TUI uses the same pattern but in a single function
//! (the CLI uses two processes / a sub-channel).

use std::path::{Path, PathBuf};

use co_scientist::agents::{Agent, AGENTS};
use tokio::sync::mpsc;

use crate::app::{Busy, ChatMsg, SharedState};
use crate::ipc::{AgentToUi, UiToAgent};

/// Drive the agent task. The caller (TUI's event loop) spawns this
/// on a tokio task; the function returns when the `rx` channel
/// closes (which happens when the UI thread drops the sender).
pub async fn run(
    db_path: PathBuf,
    initial_run_id: String,
    state: SharedState,
    mut rx: mpsc::UnboundedReceiver<UiToAgent>,
    tx: mpsc::UnboundedSender<AgentToUi>,
) {
    let mut run_id = initial_run_id;
    let mut runner: Option<co_scientist::Runner> = None;

    while let Some(msg) = rx.recv().await {
        match msg {
            UiToAgent::Shutdown => {
                on_shutdown(&db_path, &state, &mut run_id, &mut runner).await;
            }
            UiToAgent::StartSupervisor { goal } => {
                on_start_supervisor(&db_path, &state, &tx, goal).await;
            }
            UiToAgent::Turn {
                agent_name,
                user_text,
            } => {
                on_turn(
                    &db_path,
                    &mut run_id,
                    &mut runner,
                    &tx,
                    agent_name,
                    user_text,
                )
                .await;
            }
        }
    }
}

/// `UiToAgent::Shutdown`: drop the current runner, rebuild it under
/// the new `run_id` (which the UI bumped when the user hit Ctrl-N).
/// If the rebuild fails, log a system message + reset busy.
async fn on_shutdown(
    db_path: &Path,
    state: &SharedState,
    // &mut String is correct: we reassign to a new owned String
    // inside, which `&mut str` cannot express without forcing the
    // caller to allocate a fresh String per shutdown.
    #[allow(clippy::ptr_arg)]
    run_id: &mut String,
    runner: &mut Option<co_scientist::Runner>,
) {
    *runner = None;
    let new_id = {
        let s = state.lock().await;
        s.run_id.clone()
    };
    *run_id = new_id;
    if let Err(e) = rebuild_runner(db_path, run_id, runner).await {
        let mut s = state.lock().await;
        s.push_log(ChatMsg::System(format!("rebuild failed: {e}")));
        s.busy = Busy::Idle;
    }
}

/// `UiToAgent::StartSupervisor`: delegate to the supervisor-session
/// adapter (which owns the full Supervisor+Worker+Consolidation
/// wiring via the shared bundle).
async fn on_start_supervisor(
    db_path: &Path,
    state: &SharedState,
    tx: &mpsc::UnboundedSender<AgentToUi>,
    goal: String,
) {
    let sup_state = state.clone();
    let sup_tx = tx.clone();
    let sup_db = db_path.to_path_buf();
    crate::supervisor_session::start(sup_db, goal, sup_state, sup_tx);
}

/// `UiToAgent::Turn`: drive a single-agent turn end-to-end:
/// 1. Rebuild the runner if it's gone (post-Shutdown or first turn).
/// 2. Look up the agent by name; emit `TurnFailed` for unknown names.
/// 3. Emit `TurnStarted { model }` so the UI can update the model
///    label + create the streaming entry (DK11).
/// 4. Run `runner.turn_stream` with a per-turn delta channel and a
///    forwarder task that maps each delta to `AgentToUi::TurnDelta`.
///    The `tokio::join!` guarantees all deltas flush before `TurnDone`.
/// 5. Emit `TurnDone` (with cleaned text + parsed markers) or
///    `TurnFailed` (with the error).
async fn on_turn(
    db_path: &Path,
    #[allow(clippy::ptr_arg)] // &mut String correct: caller reassigns to a new String
    run_id: &mut String,
    runner: &mut Option<co_scientist::Runner>,
    tx: &mpsc::UnboundedSender<AgentToUi>,
    agent_name: String,
    user_text: String,
) {
    if runner.is_none()
        && let Err(e) = rebuild_runner(db_path, run_id, runner).await
    {
        let _ = tx.send(AgentToUi::TurnFailed {
            agent_name,
            error: format!("runner init failed: {e}"),
        });
        return;
    }
    let runner = runner.as_mut().expect("just initialized");
    let agent = match find_agent(&agent_name) {
        Some(a) => a,
        None => {
            let _ = tx.send(AgentToUi::TurnFailed {
                agent_name,
                error: "unknown agent".to_string(),
            });
            return;
        }
    };

    let _ = tx.send(AgentToUi::TurnStarted {
        model: runner.model().to_string(),
    });

    // Streaming forwarder: each text delta is forwarded to the UI as
    // it arrives from the LLM subprocess. The forwarder exits when
    // `delta_tx` is dropped (i.e. when `turn_stream` returns), so the
    // `tokio::join!` below guarantees all deltas have been forwarded
    // before we move on to send `TurnDone`.
    let (delta_tx, mut delta_rx) = mpsc::unbounded_channel::<String>();
    let forward_tx = tx.clone();
    let forward_agent = agent.name.to_string();
    let forward_deltas = async {
        while let Some(delta) = delta_rx.recv().await {
            // If the UI has dropped its receiver (Ctrl-C, etc.),
            // stop forwarding — there's no point piling up work.
            if forward_tx
                .send(AgentToUi::TurnDelta {
                    agent_name: forward_agent.clone(),
                    delta,
                })
                .is_err()
            {
                break;
            }
        }
    };

    let turn_fut = runner.turn_stream(&agent, &user_text, Some(delta_tx));
    let (turn_result, _) = tokio::join!(turn_fut, forward_deltas);

    match turn_result {
        Ok(outcome) => {
            let markers = outcome.markers.as_ref().clone();
            let _ = tx.send(AgentToUi::TurnDone {
                cleaned_text: outcome.cleaned_text,
                markers,
                agent_name: agent.name.to_string(),
            });
        }
        Err(e) => {
            let _ = tx.send(AgentToUi::TurnFailed {
                agent_name: agent.name.to_string(),
                error: format!("{e:#}"),
            });
        }
    }
}

/// Look up an `Agent` by name. The TUI's `agents` panel drives the
/// active agent via `AppState::cycle_agent`, which uses
/// `AGENTS[agent_idx]` directly. The `agent_name` from
/// `UiToAgent::Turn` is a string (e.g. "supervisor"); this is the
/// single point where we resolve it to the typed `Agent` reference.
/// Extracted from the inline `.iter().find(...).cloned()` so it can
/// be tested without a `Runner` or IPC channel.
fn find_agent(name: &str) -> Option<Agent> {
    AGENTS.iter().find(|a| a.name == name).cloned()
}

/// Construct a fresh `Runner` for the given `run_id`. Used by the
/// first `Turn` after startup and by the `Shutdown` arm to pick up
/// the new run id (Ctrl-N).
async fn rebuild_runner(
    db_path: &Path,
    run_id: &str,
    slot: &mut Option<co_scientist::Runner>,
) -> anyhow::Result<()> {
    let conn = co_scientist::db::Db::connect_fresh(db_path.to_str().unwrap()).await?;
    let d = co_scientist::Db::new(conn);
    let mem = co_scientist::Memory::new(d);
    *slot = Some(co_scientist::Runner::new(
        mem,
        run_id.to_string(),
        co_scientist::runner::RunnerConfig::default(),
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::find_agent;
    use co_scientist::agents::AGENTS;

    #[test]
    fn find_agent_returns_known_agent() {
        // Every AGENTS entry should round-trip through find_agent.
        for a in AGENTS {
            assert_eq!(find_agent(a.name).map(|x| x.name), Some(a.name));
        }
    }

    #[test]
    fn find_agent_returns_none_for_unknown_name() {
        // The bug-prone case: a UI bug or stale agent name
        // should NOT panic — the caller emits TurnFailed instead.
        assert!(find_agent("not-a-real-agent").is_none());
        assert!(find_agent("").is_none());
    }

    /// Sanity: the agent name the TUI gets from
    /// `AppState::current_agent_name` always matches a real agent.
    /// This is the invariant the `Turn` arm depends on.
    #[test]
    fn all_known_agent_names_resolve() {
        let names: Vec<&str> = AGENTS.iter().map(|a| a.name).collect();
        for n in names {
            assert!(find_agent(n).is_some(), "agent {n} did not resolve");
        }
    }
}
