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
//! The CLI's `cmd_start` (`co-scientist/src/main.rs`) wires this up.
//! The shape is reusable: any future consumer that needs the full
//! supervisor + consolidation + worker stack adopts the same wiring.
//! This module is the seam.
//!
//! ## What this module is *not*
//!
//! - It is not a `Subscriber` trait. We considered a `Subscriber`-shaped
//!   abstraction so the bus forwarder could be polymorphic, but the only
//!   real consumer is `mpsc::UnboundedSender<MemoryEvent>` — the CLI uses
//!   it directly via `spawn_bus_forwarder`. A trait would wrap one adapter,
//!   which the codebase-design vocabulary flags as a hypothetical seam.
//!   If a second consumer ever needs different forwarding, that's the
//!   moment to introduce the trait.
//! - It does not modify `Supervisor`, `Worker`, or `ConsolidationService`.
//!   The 28 pre-existing clippy warnings in `supervisor.rs` and `runner.rs`
//!   are out of scope. This
//!   module is a *new* file and is clippy-clean by construction.
//!
//! ## Single-adapter seam (justification)
//!
//! The deletion test: would deleting this module and inlining the wiring
//! back into the caller reduce complexity? No — it would concentrate
//! ~80 lines of plumbing at the CLI call site, and any future consumer
//! adopting the same pattern would drift (the bus forwarder would diverge,
//! the shutdown bridge would diverge, etc.). The module *concentrates*
//! the wiring, so it earns its place.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::bus::{run_failure_aggregator, EventBus, FailureAggregatorConfig, MemoryEvent};
use crate::db::Db;
use crate::memory::Memory;
use crate::prompts::Prompts;
use crate::promotion::{ConsolidationService, PromotionConfig};
use crate::queue::TaskQueue;
use crate::registry::ToolRegistry;
use crate::run_agent::{RunAgentTool, SessionRunners};
use crate::runner::RunnerConfig;
use crate::supervisor::{Supervisor, SupervisorConfig};
use crate::tool::builtin_tools;
use crate::worker::{run_worker, WorkerConfig};

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
/// `shutdown` is the only shutdown signal the bundle listens on. The
/// caller is responsible for constructing it — typically with
/// `co_scientist::worker::ctrl_c_shutdown_pair()` (which gives you a
/// `watch::Receiver<bool>` that flips on Ctrl+C) and an additional
/// external stop signal merged into the same channel via
/// `watch::Sender::send`.
///
/// **Why the bundle doesn't create its own Ctrl+C handler**:
/// the original bundle design (C1) created an internal
/// `ctrl_c_shutdown_pair` and bridged the caller's `external_stop`
/// into it. That meant the CLI's call to `ctrl_c_shutdown_pair()` for
/// the bundle's `external_stop` parameter would *race* with the
/// bundle's internal pair — two tokio tasks listening for SIGINT.
/// Both fire, both forward, all correct, but wasteful and confusing.
/// The new design trusts the caller to own SIGINT handling. The CLI's
/// `cmd_start` builds the pair directly; the bundle sees a single receiver.
///
/// ## Event bus
///
/// The caller constructs the `EventBus` and passes it in. This is
/// deliberate: it lets the caller subscribe to the same bus the
/// supervisor / worker / consolidation publish to, so live telemetry
/// (task progress, memory writes) reaches the front-end without the
/// bundle needing a generic `Subscriber` trait. The CLI uses this to
/// stream live `MemoryEvent`s without polling SQLite via
/// [`spawn_bus_forwarder`].
pub async fn run(
    db_path: PathBuf,
    event_bus: EventBus,
    session_id: String,
    goal: String,
    preferences: String,
    cfg: Config,
    shutdown: watch::Receiver<bool>,
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
    // One SessionRunners cache, shared between RunAgentTool (the
    // worker dispatch path) and Supervisor::finalize (the final
    // metareview). The supervisor's metareview reuses the cached
    // Runner so it doesn't spawn a second Claude subprocess for a
    // role already cached by an earlier idle-injection dispatch.
    let session_runners = Arc::new(SessionRunners::new());
    let reg = build_registry(&q, &prompts, &cfg.runner, &session_runners)?;
    let reg = Arc::new(reg);

    // The bundle does not own SIGINT — see the doc comment above for
    // why. We bridge the caller's `shutdown` into an internal pair
    // (which has NO SIGINT listener of its own) so the worker /
    // consolidation / supervisor all observe the same shutdown, and
    // the bundle can also signal wind-down via the internal `Sender`.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    {
        let shutdown_tx = shutdown_tx.clone();
        let mut shutdown = shutdown;
        tokio::spawn(async move {
            while shutdown.changed().await.is_ok() {
                if *shutdown.borrow() {
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

    // Failure aggregator: subscribes to MemoryEvent::MarkerFailed on the
    // bus and republishes a periodic MemoryEvent::FailureStats. Without
    // this, MarkerFailed events published by Runner::dispatch_marker
    // have no consumer — the broadcast channel is created but no one
    // ever listens, so per-agent failure modes are invisible at runtime.
    let aggregator_handle = tokio::spawn({
        let shutdown = shutdown_rx.clone();
        let bus = event_bus.clone();
        async move {
            run_failure_aggregator(bus, FailureAggregatorConfig::default(), shutdown).await;
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
        session_runners.clone(),
    )
    .await;

    // Wind down the background tasks. We do this *after* the supervisor
    // returns regardless of `Ok`/`Err` — even an error shouldn't leave
    // the worker and consolidation looping forever.
    let _ = shutdown_tx.send(true);
    let _ = worker_handle.await;
    let _ = consolidation_handle.await;
    let _ = aggregator_handle.await;

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
    session_runners: &Arc<SessionRunners>,
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

    // `RunAgentTool` shares `session_runners` with the supervisor so
    // `Supervisor::finalize` can reuse the cached Runner for the
    // metareview role.
    let run_agent_tool = RunAgentTool::with_session_runners(
        queue.clone(),
        prompts.clone(),
        Arc::new(reg.clone()),
        runner_cfg.clone(),
        session_runners.clone(),
    );
    reg.register(Arc::new(run_agent_tool));

    Ok(reg)
}

/// Drain `event_bus` and forward each `MemoryEvent` to the caller-supplied
/// `tx`. The CLI calls this with a passthrough or `tracing`-emitting
/// closure that observes live `MemoryEvent`s without polling SQLite.
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
