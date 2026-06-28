//! `SupervisorBundle` — one constructor for the full Supervisor + Worker +
//! Consolidation stack.
//!
//! ## Why this exists
//!
//! Before this module, every front-end that wanted to run a full research
//! session had to wire the same five components in the same order:
//!
//! 1. Open N fresh `Db` connections (rusqlite forbids sharing one `Connection`
//!    across concurrent components).
//! 2. Build a `ToolRegistry` seeded with `builtin_tools()`, any on-disk
//!    skills, and a `RunAgentTool` that takes a clone of the registry and
//!    the prompts.
//! 3. Construct an `EventBus` shared by three `Memory` instances
//!    (supervisor, worker, consolidation).
//! 4. Spawn the consolidation service as a background tokio task.
//! 5. Spawn the worker as a background tokio task.
//! 6. Run the supervisor (blocking) on the same bus + shutdown channel.
//! 7. On return, signal shutdown to the background tasks and join them.
//!
//! The TUI's `run_supervisor_inner` (formerly in `co-scientist-tui/src/main.rs`)
//! and the CLI's `cmd_start` (`co-scientist/src/main.rs`) did exactly this.
//! Two adapters, one shared shape — a real seam with a real seam name. This
//! module is the seam name.
//!
//! ## What this module is *not*
//!
//! - It is not a `Subscriber` trait. We considered a `Subscriber`-shaped
//!   abstraction so the bus forwarder could be polymorphic, but the only
//!   two real consumers (`mpsc::UnboundedSender<MemoryEvent>` and
//!   `mpsc::UnboundedSender<AgentToUi::SupervisorEvent>`) share the same
//!   shape already. A trait would be one adapter, which the codebase-design
//!   vocabulary flags as a hypothetical seam. If a third consumer ever
//!   needs different forwarding, that's the moment to introduce the trait.
//! - It does not modify `Supervisor`, `Worker`, or `ConsolidationService`.
//!   The 28 pre-existing clippy warnings in `supervisor.rs` and `runner.rs`
//!   are out of scope for TUI redesign work (D5 in project memory). This
//!   module is a *new* file and is clippy-clean by construction.
//!
//! ## Two-adapter seam (justification)
//!
//! The deletion test: would deleting this module and inlining the wiring
//! back into the two callers reduce complexity? No — it would concentrate
//! the same ~80 lines of plumbing in two places, and the two copies would
//! drift (the TUI's bus forwarder would diverge from the CLI's stdout
//! subscriber, the shutdown bridge would diverge, etc.). The module
//! *concentrates* the wiring, so it earns its place.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::bus::{EventBus, MemoryEvent};
use crate::db::Db;
use crate::memory::Memory;
use crate::prompts::Prompts;
use crate::promotion::{ConsolidationService, PromotionConfig};
use crate::queue::TaskQueue;
use crate::registry::ToolRegistry;
use crate::run_agent::RunAgentTool;
use crate::runner::RunnerConfig;
use crate::supervisor::{Supervisor, SupervisorConfig};
use crate::tool::builtin_tools;
use crate::worker::{ctrl_c_shutdown_pair, run_worker, WorkerConfig};

/// Configuration for the full supervisor stack. Only the pieces that
/// *vary* between calls are exposed; everything else stays at the
/// crate's defaults so callers don't have to re-derive them.
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub supervisor: SupervisorConfig,
    pub promotion: PromotionConfig,
    pub runner: RunnerConfig,
    pub worker: WorkerConfig,
}

/// Spawn the supervisor stack and run it to completion. Returns the
/// supervisor's `Result<()>` (or a fatal wiring error).
///
/// ## Shutdown signaling
///
/// `external_stop` is the caller's external shutdown signal (the TUI's
/// `/stop` slash command pipes into it). The bundle bridges it into the
/// internal `ctrl_c_shutdown_pair` so a `/stop` from the UI and a Ctrl+C
/// from the terminal both wind the pipeline down the same way. The
/// caller never needs to know about the internal pair.
///
/// ## Event bus
///
/// The caller constructs the `EventBus` and passes it in. This is
/// deliberate: it lets the caller subscribe to the same bus the
/// supervisor / worker / consolidation publish to, so live telemetry
/// (task progress, memory writes) reaches the front-end without the
/// bundle needing a generic `Subscriber` trait. The TUI uses this to
/// forward `MemoryEvent → AgentToUi::SupervisorEvent` via
/// [`spawn_bus_forwarder`].
pub async fn run(
    db_path: PathBuf,
    event_bus: EventBus,
    session_id: String,
    goal: String,
    preferences: String,
    cfg: Config,
    external_stop: watch::Receiver<bool>,
) -> Result<BundleOutcome> {
    // Open one connection here only to confirm the DB is reachable. The
    // supervisor, worker, and consolidation each open their OWN fresh
    // connection below because `rusqlite::Connection` is single-threaded
    // and sharing one across concurrent components triggers "concurrent
    // use forbidden".
    let _d = crate::db::open(db_path.to_str().unwrap())
        .await
        .context("opening initial DB connection for bundle")?;

    let mem = Memory::with_bus(
        Db::new(Db::connect_fresh(db_path.to_str().unwrap()).await?),
        event_bus.clone(),
    );
    let worker_mem = Memory::with_bus(
        Db::new(Db::connect_fresh(db_path.to_str().unwrap()).await?),
        event_bus.clone(),
    );
    let consolidation_mem = Memory::with_bus(
        Db::new(Db::connect_fresh(db_path.to_str().unwrap()).await?),
        event_bus.clone(),
    );

    let q = TaskQueue::new(Db::new(Db::connect_fresh(db_path.to_str().unwrap()).await?));

    let prompts = Arc::new(Prompts::new()?);
    let reg = build_registry(&q, &prompts, &cfg.runner)?;
    let reg = Arc::new(reg);

    let (shutdown_tx, shutdown_rx) = ctrl_c_shutdown_pair();

    // Bridge the caller's external stop signal (e.g. TUI `/stop`) into
    // the internal shutdown channel. When `external_stop` flips to
    // `true`, we mirror it onto `shutdown_tx` so the worker /
    // consolidation / supervisor all observe the same shutdown.
    {
        let shutdown_tx = shutdown_tx.clone();
        let mut external_stop = external_stop;
        tokio::spawn(async move {
            while external_stop.changed().await.is_ok() {
                if *external_stop.borrow() {
                    let _ = shutdown_tx.send(true);
                    break;
                }
            }
        });
    }

    let consolidation_handle = tokio::spawn({
        let shutdown = shutdown_rx.clone();
        let bus = event_bus.clone();
        async move {
            let svc = ConsolidationService::new(consolidation_mem, cfg.promotion);
            if let Err(e) = svc.run(bus, shutdown).await {
                tracing::error!(error = %e, "consolidation service failed");
            }
        }
    });

    let worker_handle = tokio::spawn({
        let shutdown = shutdown_rx.clone();
        let q = q.clone();
        let reg = reg.clone();
        async move {
            if let Err(e) =
                run_worker(worker_mem, q, reg, cfg.worker, shutdown).await
            {
                tracing::error!(error = %e, "worker failed");
            }
        }
    });

    let supervisor_result = Supervisor::run(
        mem.clone(),
        q.clone(),
        reg.clone(),
        prompts,
        cfg.supervisor,
        session_id.clone(),
        goal,
        preferences,
        shutdown_rx.clone(),
        shutdown_tx.clone(),
    )
    .await;

    // Wind down the background tasks. We do this *after* the supervisor
    // returns regardless of `Ok`/`Err` — even an error shouldn't leave
    // the worker and consolidation looping forever.
    let _ = shutdown_tx.send(true);
    let _ = worker_handle.await;
    let _ = consolidation_handle.await;

    Ok(BundleOutcome {
        supervisor: supervisor_result,
        session_id,
    })
}

/// Result of a completed bundle run. `supervisor` is the inner
/// `Supervisor::run` result; `session_id` echoes the input so the caller
/// can correlate.
#[derive(Debug)]
pub struct BundleOutcome {
    pub supervisor: Result<()>,
    pub session_id: String,
}

/// Build the `ToolRegistry` (builtin + skills + `RunAgentTool`).
/// `queue` and `prompts` are passed in so the `RunAgentTool` registers
/// against the same queue/prompts the supervisor will dispatch to.
fn build_registry(
    queue: &TaskQueue,
    prompts: &Arc<Prompts>,
    runner_cfg: &RunnerConfig,
) -> Result<ToolRegistry> {
    let mut reg = ToolRegistry::new();
    reg.register_all(builtin_tools());

    let skills_dir = std::path::PathBuf::from(
        std::env::var("CO_SCIENTIST_SKILLS")
            .unwrap_or_else(|_| "co_scientist_skills".to_string()),
    );
    if skills_dir.exists() {
        for s in crate::discover_skills(&skills_dir)? {
            reg.register(crate::skill_to_tool(s));
        }
    }

    let run_agent_tool = RunAgentTool::new(
        queue.clone(),
        prompts.clone(),
        Arc::new(reg.clone()),
        runner_cfg.clone(),
    );
    reg.register(Arc::new(run_agent_tool));

    Ok(reg)
}

/// Drain `event_bus` and forward each `MemoryEvent` to the caller-supplied
/// `tx`. The TUI calls this with a closure that maps `MemoryEvent →
/// AgentToUi::SupervisorEvent` (one adapter); the CLI can call it with
/// a passthrough or `tracing`-emitting closure (the second adapter — the
/// seam earns its keep).
///
/// `RecvError::Lagged` is handled by skipping the gap silently, matching
/// the pre-extraction behavior in `run_supervisor_inner`. Drop the returned
/// `JoinHandle` to abort the forwarder (it exits naturally when `event_bus`'s
/// last sender is dropped).
pub fn spawn_bus_forwarder<T, F>(
    event_bus: EventBus,
    tx: tokio::sync::mpsc::UnboundedSender<T>,
    map: F,
) -> JoinHandle<()>
where
    T: Send + 'static,
    F: Fn(MemoryEvent) -> T + Send + 'static,
{
    let mut rx = event_bus.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    if tx.send(map(ev)).is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}
