//! TUI-specific supervisor session adapter.
//!
//! This module is the **second adapter** that justifies the
//! [`co_scientist::supervisor_bundle`] seam. The bundle handles the
//! generic "spawn supervisor + worker + consolidation" plumbing; this
//! adapter handles the TUI-specific bits:
//!
//! - **Session ID** generation (`co_scientist::memory::new_run_id()`).
//! - **EventBus construction** — created here, handed to the bundle
//!   which uses it to share state across supervisor / worker /
//!   consolidation. The TUI also subscribes via
//!   `supervisor_bundle::spawn_bus_forwarder` to receive
//!   `MemoryEvent → AgentToUi::SupervisorEvent`.
//! - **Started/Finished/Failed IPC messages** sent through `tx`
//!   (`AgentToUi::SupervisorStarted` / `Finished` / `Failed`).
//! - **The external stop signal** — a `watch::channel(false)` created
//!   here, the `Sender` half sent to the UI in `SupervisorStarted` so
//!   `/stop` can flip it, the `Receiver` half passed to the bundle so
//!   the bundle can bridge it into its internal `ctrl_c_shutdown_pair`.
//!
//! ## Why this is an adapter (and not part of the bundle)
//!
//! The bundle sits in `co-scientist` and must not depend on
//! `co-scientist-tui` (D2 in project memory). Anything that touches
//! `AgentToUi` (an `ipc.rs` type in the TUI crate) lives here.
//!
//! ## Test surface
//!
//! The TUI's `handle_agent_msg` is exercised by the existing scroll
//! and scrub tests. The bundle's `run` function is exercised by
//! `co-scientist`'s own tests (and by the CLI's `cmd_start` smoke
//! tests). This module is the glue — a thin file with no logic that
//! benefits from a test in isolation. If it grows, factor.

use std::path::PathBuf;

use co_scientist::supervisor_bundle::{self, Config as BundleConfig};
use tokio::sync::mpsc;
use tokio::sync::watch;

use crate::app::SharedState;
use crate::ipc::AgentToUi;

/// Top-level entry point. Spawns the bundle in a background tokio task
/// and returns immediately. The function handles:
/// 1. Constructing the `EventBus` shared by supervisor/worker/
///    consolidation. The TUI also subscribes via the bus forwarder so
///    live telemetry (task progress, memory writes) flows into the
///    chat log + sidebars through `AgentToUi::SupervisorEvent`.
/// 2. Sending `AgentToUi::SupervisorStarted` to the UI (so `/stop` can
///    hold the `stop_tx` end and the chat log gets the "session started"
///    line).
/// 3. Sending the matching `Finished` or `Failed` message when the
///    bundle returns.
///
/// `state` is the TUI's `SharedState`. It's currently unused — the
/// pre-extraction version took it for symmetry with `run_agent_task`
/// but never read it. We keep the parameter (with `let _ = state;`) so
/// the call site in `run_agent_task` doesn't need to change.
pub fn start(
    db_path: PathBuf,
    goal: String,
    state: SharedState,
    tx: mpsc::UnboundedSender<AgentToUi>,
) {
    let _ = state; // reserved for future use — see doc comment
    let session_id = co_scientist::memory::new_run_id();
    let (stop_tx, stop_rx) = watch::channel(false);

    // Tell the UI the session is alive. The UI stores `stop_tx` in
    // `AppState.supervisor_stop_tx` so `/stop` can flip it.
    let _ = tx.send(AgentToUi::SupervisorStarted {
        session_id: session_id.clone(),
        stop_tx,
    });

    // Construct the bus and wire the TUI's bus forwarder so live
    // telemetry (task progress, memory writes) flows into the chat
    // log and sidebars. The bundle gets a clone of the same bus so
    // the supervisor / worker / consolidation share it.
    let bus = co_scientist::bus::EventBus::default();
    let _bus_forwarder = supervisor_bundle::spawn_bus_forwarder(
        bus.clone(),
        tx.clone(),
        AgentToUi::SupervisorEvent,
    );

    // Build the bundle's shutdown channel. The bundle trusts the
    // caller to own SIGINT handling (see supervisor_bundle doc); we
    // bridge both `/stop` (from `stop_rx`) and Ctrl+C (from the TUI
    // main event loop's key handler — which is what calls
    // `tx_to_agent.send(UiToAgent::Shutdown)` on Ctrl-C) into a
    // single `watch::Receiver` for the bundle. Note: the
    // `ctrl_c_shutdown_pair` we use here is for **process-level**
    // SIGINT (in case the TUI event loop misses the key event under
    // load); the TUI's main loop handles the in-band Ctrl-C key
    // separately by quitting the whole app.
    let (bundle_shutdown_tx, bundle_shutdown_rx) =
        co_scientist::worker::ctrl_c_shutdown_pair();
    {
        let bundle_shutdown_tx = bundle_shutdown_tx.clone();
        let mut stop_rx = stop_rx;
        tokio::spawn(async move {
            while stop_rx.changed().await.is_ok() {
                if *stop_rx.borrow() {
                    let _ = bundle_shutdown_tx.send(true);
                    break;
                }
            }
        });
    }

    // Run the bundle in a background task. The bundle's `run` blocks
    // until the supervisor returns (or is signalled to stop), so this
    // task lives for the entire session.
    let tx_for_done = tx.clone();
    tokio::spawn(async move {
        let outcome = supervisor_bundle::run(
            db_path,
            bus,
            session_id.clone(),
            goal,
            String::new(), // preferences: TUI doesn't expose this yet
            BundleConfig::default(),
            bundle_shutdown_rx,
        )
        .await;

        match outcome {
            Ok(bundle_outcome) => {
                let reason = match bundle_outcome.supervisor {
                    Ok(()) => "ok".to_string(),
                    Err(e) => format!("error: {e:#}"),
                };
                let _ = tx_for_done.send(AgentToUi::SupervisorFinished {
                    reason,
                    session_id,
                });
            }
            Err(e) => {
                let _ = tx_for_done.send(AgentToUi::SupervisorFailed {
                    error: format!("{e:#}"),
                });
            }
        }
    });
}
