/// The automated research supervisor.
///
/// Takes a research goal, drives the full pipeline (generation → reflection
/// → ranking → evolution → meta-review), and terminates with a final report.
/// No human intervention required after launch.
///
/// The supervisor is a lightweight monitor: it enqueues tasks and watches
/// the queue. Agent execution and follow-up chaining are handled by the
/// `RunAgentTool` (registered in the registry), so the worker dispatches
/// agent tasks just like any other tool.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use serde_json::json;
use tokio::sync::watch;
use tracing::{debug, error, info, warn};

use crate::agents::AGENTS;
use crate::hypothesis::HypothesisRepo;
use crate::memory::Memory;
use crate::policies::{IdlePolicy, RunCounters, RunSnapshot, TerminationDecision, TerminationPolicy};
use crate::prompts::{AgentMode, Prompts, PromptContext};
use crate::queue::{EnqueueRequest, TaskQueue};
use crate::research_session::ResearchSessionRepo;
use crate::run_agent::{IdleSpec, IdleState, SessionRunners, IDLE_SPECS};
use crate::tournament::matches::TournamentRepo;

#[derive(Debug, Clone)]
pub struct SupervisorConfig {
    pub budget_usd: f64,
    pub deadline: Duration,
    pub concurrency: usize,
    pub stability_threshold: usize,
    pub stability_epsilon: f64,
    pub min_hypotheses: usize,
    pub min_mature: usize,
    pub meta_review_interval: usize,
    pub n_initial: usize,
    pub initial_elo: f64,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            budget_usd: 0.0,
            deadline: Duration::ZERO,
            concurrency: 4,
            stability_threshold: 3,
            stability_epsilon: 25.0,
            min_hypotheses: 2,
            min_mature: 20,
            meta_review_interval: 50,
            n_initial: 3,
            initial_elo: 1200.0,
        }
    }
}

pub struct Supervisor {
    memory: Memory,
    queue: TaskQueue,
    registry: Arc<crate::registry::ToolRegistry>,
    prompts: Arc<Prompts>,
    config: SupervisorConfig,
    repo: ResearchSessionRepo,
    idle_policy: IdlePolicy,
    termination_policy: TerminationPolicy,
    /// Per-session Runner cache. Shared with `RunAgentTool` when
    /// `finalize()` runs the final metareview — keeps the supervisor
    /// from spawning a second Claude subprocess for a role already
    /// cached by the worker's run_agent dispatch.
    session_runners: Arc<SessionRunners>,
    session_id: String,
    goal: String,
    preferences: String,
    start_time: Instant,
    stability_snapshots: Vec<Vec<(i64, f64)>>,
    last_meta_review_at: usize,
}

impl Supervisor {
    pub async fn run(
        memory: Memory,
        queue: TaskQueue,
        registry: Arc<crate::registry::ToolRegistry>,
        prompts: Arc<Prompts>,
        config: SupervisorConfig,
        session_id: String,
        goal: String,
        preferences: String,
        mut shutdown: watch::Receiver<bool>,
        shutdown_tx: watch::Sender<bool>,
        session_runners: Arc<SessionRunners>,
    ) -> Result<()> {
        let repo = ResearchSessionRepo::new(memory.db_arc());

        // Startup recovery: any `running` sessions from a prior process
        // that died (SIGKILL, OOM, crash) without running finalize()
        // leave their row stuck at status='running' forever. Mark them
        // 'interrupted' so the DB doesn't accumulate zombies.
        let recovered = repo
            .recover_stale(&session_id)
            .await
            .context("recovering stale running sessions")?;
        if recovered > 0 {
            info!(recovered, "marked stale running sessions as interrupted");
        }

        // Cancel any pending tasks that survived from prior runs (e.g.
        // a reflection task enqueued milliseconds before finalize() ran).
        let cancelled = repo
            .cancel_orphaned_tasks()
            .await
            .context("recovering stale tasks")?;
        if cancelled > 0 {
            info!(cancelled, "cancelled stale pending/leased tasks");
        }

        // Persist the research session.
        repo.create(
            &session_id,
            &goal,
            &preferences,
            if config.budget_usd > 0.0 {
                Some(config.budget_usd)
            } else {
                None
            },
            &chrono::Utc::now().to_rfc3339(),
        )
        .await
        .context("creating research session")?;

        let mut sup = Supervisor {
            memory: memory.clone(),
            queue: queue.clone(),
            registry: registry.clone(),
            prompts: prompts.clone(),
            config: config.clone(),
            repo: repo.clone(),
            idle_policy: IdlePolicy::default(),
            termination_policy: TerminationPolicy::new(),
            session_runners: session_runners.clone(),
            session_id: session_id.clone(),
            goal: goal.clone(),
            preferences: preferences.clone(),
            start_time: Instant::now(),
            stability_snapshots: Vec::new(),
            last_meta_review_at: 0,
        };

        info!(session = %session_id, goal = %goal, "starting research session");

        // Phase 1: Seed the pipeline.
        sup.enqueue_initial_tasks().await?;

        // Phase 2: Main loop.
        let mut bus_rx = memory.bus().subscribe();
        // Delay first idle tick by 30s so the initial pipeline has time
        // to produce results before idle injection kicks in.
        let mut idle_tick = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_secs(30),
            Duration::from_secs(10),
        );
        let mut tasks_completed: usize = 0;
        let mut last_completion = Instant::now();

        loop {
            if *shutdown.borrow() {
                info!("shutdown requested");
                break;
            }

            if let Some(reason) = sup.check_termination().await? {
                info!(reason = %reason, "termination condition met");
                break;
            }

            tokio::select! {
                ev = bus_rx.recv() => {
                    match ev {
                        Ok(crate::bus::MemoryEvent::TaskCompleted { .. }) => {
                            tasks_completed += 1;
                            last_completion = Instant::now();
                            debug!("task completed (total={tasks_completed})");
                        }
                        Ok(crate::bus::MemoryEvent::TaskFailed { task_id, error, .. }) => {
                            warn!(task_id = %task_id, error = %error, "task failed");
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            warn!(skipped = n, "event bus lagged");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
                _ = idle_tick.tick() => {
                    let pending = queue.pending_count(&session_id).await? as usize;
                    let inflight = queue.inflight_count(&session_id).await? as usize;
                    let since_last = last_completion.elapsed();
                    // Only inject idle work when:
                    // 1. Queue is truly empty (no pending, no inflight)
                    // 2. At least one task has completed (initial pipeline drained)
                    // 3. 10s grace period since last completion (avoid race with claim)
                    let counters = RunCounters { tasks_completed };
                    if matches!(
                        sup.idle_policy.should_inject(counters, pending, inflight, since_last),
                        crate::policies::InjectDecision::SpawnReflectionAgent
                    ) {
                        let injected = sup.decide_next_steps().await?;
                        if injected == 0 {
                            info!("no more work, terminating");
                            break;
                        }
                    }
                }
                _ = shutdown.changed() => {
                    info!("shutdown signal received");
                    break;
                }
            }
        }

        // Phase 3: Finalize.
        sup.finalize().await?;
        info!(session = %session_id, "research session complete");
        // Signal worker + consolidation to exit. They watch this same
        // receiver for shutdown. Without this, they wait forever on
        // tokio::select! until a Ctrl+C arrives.
        let _ = shutdown_tx.send(true);
        Ok(())
    }

    /// Enqueue the initial pipeline tasks: parse goal + N generation tasks.
    async fn enqueue_initial_tasks(&mut self) -> Result<()> {
        // Supervisor parses the goal.
        let mut ctx = PromptContext::new();
        ctx.set("goal", &self.goal);
        ctx.set("preferences", &self.preferences);
        let rendered = self.prompts.render(AgentMode::ParseGoal, &ctx)?;
        self.queue
            .enqueue(EnqueueRequest {
                session_id: self.session_id.clone(),
                agent: "supervisor".into(),
                action: "run_agent".into(),
                payload: json!({
                    "agent": "supervisor",
                    "mode": "parse_goal",
                    "prompt": rendered,
                }),
                priority: 100,
                max_attempts: 3,
            })
            .await?;
        info!("enqueued supervisor/parse_goal");

        // N initial generation tasks. Pass empty prompt — RunAgentTool
        // will build it from context (goal + system feedback).
        for i in 0..self.config.n_initial {
            // Include `iteration` in the payload so the idempotency key
            // differs across loop iterations. Without this, all N tasks
            // share the same payload hash and `ON CONFLICT DO NOTHING`
            // silently dedupes them down to one row.
            self.queue
                .enqueue(EnqueueRequest {
                    session_id: self.session_id.clone(),
                    agent: "generation".into(),
                    action: "run_agent".into(),
                    payload: json!({
                        "agent": "generation",
                        "mode": "generation_literature",
                        "prompt": "",
                        "goal": self.goal,
                        "preferences": self.preferences,
                        "iteration": i,
                    }),
                    priority: 100,
                    max_attempts: 3,
                })
                .await?;
            info!(i = i + 1, n = self.config.n_initial, "enqueued generation task");
        }

        Ok(())
    }

    /// Inject work when the queue drains. Iterates [`IDLE_SPECS`] —
    /// the idle half of the DAG — and enqueues any row whose
    /// predicate fires. Folds the previous 110-line if-tree into a
    /// single data-driven loop. Returns the number of tasks injected.
    async fn decide_next_steps(&self) -> Result<usize> {
        let state = self.build_idle_state().await?;
        let mut injected = 0usize;
        for spec in IDLE_SPECS {
            if let Some(extra) = (spec.predicate)(&state) {
                self.enqueue_idle(spec, &state, extra).await?;
                injected += 1;
            }
        }
        Ok(injected)
    }

    /// Build the [`IdleState`] snapshot the [`IDLE_SPECS`] predicates
    /// evaluate against. One DB roundtrip per dimension — total count,
    /// mature count, top-N, match count, experimentable hypothesis.
    /// Previously inlined into `decide_next_steps`.
    async fn build_idle_state(&self) -> Result<IdleState> {
        let hyp_repo = HypothesisRepo::new(self.memory.db_arc());
        let tour_repo = TournamentRepo::new(self.memory.db_arc());
        let total = hyp_repo.total_count(&self.session_id).await?;
        let matches = tour_repo.match_count(&self.session_id).await? as usize;
        let mature = hyp_repo.mature_count(&self.session_id, 3).await? as usize;
        let top = hyp_repo.top_n(&self.session_id, 5).await?;
        let hypothesis_needing_experiment = self
            .pick_hypothesis_needing_experiment()
            .await?
            .and_then(|(hyp_id, _)| Some(hyp_id));
        Ok(IdleState {
            session_id: self.session_id.clone(),
            goal: self.goal.clone(),
            preferences: self.preferences.clone(),
            total_hypotheses: total,
            mature_hypotheses: mature,
            match_count: matches,
            last_meta_review_at: self.last_meta_review_at,
            meta_review_interval: self.config.meta_review_interval,
            min_hypotheses: self.config.min_hypotheses,
            min_mature: self.config.min_mature,
            top_hypotheses_empty: top.is_empty(),
            hypothesis_needing_experiment,
        })
    }

    /// Enqueue one row from [`IDLE_SPECS`]. The standard
    /// `(agent, mode, prompt, goal, preferences)` fields are added by
    /// the supervisor; the spec's `predicate` returned the extra
    /// payload fields (e.g. `hypothesis_id` for experiment_design).
    async fn enqueue_idle(
        &self,
        spec: &IdleSpec,
        state: &IdleState,
        extra: serde_json::Value,
    ) -> Result<()> {
        let mut payload = json!({
            "agent": spec.next_agent,
            "mode": spec.next_mode,
            "prompt": "",
            "goal": state.goal,
            "preferences": state.preferences,
        });
        if let Some(obj) = extra.as_object() {
            for (k, v) in obj {
                payload[k] = v.clone();
            }
        }
        self.queue
            .enqueue(EnqueueRequest {
                session_id: self.session_id.clone(),
                agent: spec.next_agent.to_string(),
                action: "run_agent".to_string(),
                payload,
                priority: spec.priority,
                max_attempts: 3,
            })
            .await?;
        info!("injected {}", spec.label);
        Ok(())
    }

    /// Pick a hypothesis that has been reviewed at least once but
    /// has no experiment yet. Returns (hypothesis_id, latest_review_id)
    /// so the caller can log if needed. Greedy: picks the
    /// lowest-Elo reviewed hypothesis to push weak claims through
    /// the empirical loop first.
    async fn pick_hypothesis_needing_experiment(&self) -> Result<Option<(i64, Option<i64>)>> {
        let conn = self.memory.db().conn();
        // A hypothesis is "experimentable" if it has at least one
        // review AND no experiment row yet. The `latest_experiment_id`
        // column lets us skip hypotheses that already have one.
        let mut rows = conn
            .query(
                "SELECT h.id, h.elo
                 FROM hypotheses h
                 WHERE h.session_id = ?1
                   AND h.state IN ('reviewed', 'in_tournament', 'ranked')
                   AND h.latest_experiment_id IS NULL
                   AND EXISTS (
                       SELECT 1 FROM semantic_memories s
                       WHERE s.run_id = h.session_id
                         AND s.scope = 'review'
                         AND s.details_json LIKE '%' || CAST(h.id AS TEXT) || '%'
                   )
                 ORDER BY h.elo ASC
                 LIMIT 1",
                [self.session_id.as_str()],
            )
            .await?;
        if let Some(row) = rows.next().await? {
            Ok(Some((row.get(0)?, None)))
        } else {
            Ok(None)
        }
    }

    /// Check termination conditions. Returns `Some(reason)` if done.
    async fn check_termination(&mut self) -> Result<Option<String>> {
        // Compute budget spent from the events table.
        let budget_spent = if self.config.budget_usd > 0.0 {
            let mut rows = self
                .memory
                .db()
                .conn()
                .query(
                     "SELECT COALESCE(SUM(CAST(json_extract(payload_json, '$.raw_len') AS INTEGER)), 0)
                     FROM events WHERE run_id = ?1 AND type = 'turn_completed'",
                    [self.session_id.as_str()],
                )
                .await?;
            if let Some(row) = rows.next().await? {
                let chars: i64 = row.get(0)?;
                (chars as f64 / 4.0) * 3.0 / 1_000_000.0
            } else {
                0.0
            }
        } else {
            0.0
        };

        // Build a snapshot of the top hypotheses for Elo stability check.
        let hyp_repo = HypothesisRepo::new(self.memory.db_arc());
        let tour_repo = TournamentRepo::new(self.memory.db_arc());
        let top = hyp_repo.top_n(&self.session_id, 5).await?;
        let match_count = tour_repo.match_count(&self.session_id).await? as usize;
        let top_pairs: Vec<(i64, f64)> = if top.len() >= self.config.min_hypotheses {
            top.iter().map(|h| (h.id, h.elo)).collect()
        } else {
            Vec::new()
        };
        let previous_snapshot = self.stability_snapshots.last().cloned();

        let snap = RunSnapshot {
            elapsed: self.start_time.elapsed(),
            deadline: self.config.deadline,
            budget_usd: self.config.budget_usd,
            budget_spent_usd: budget_spent,
            top_hypotheses: top_pairs.clone(),
            min_hypotheses: self.config.min_hypotheses,
            stability_epsilon: self.config.stability_epsilon,
            stability_threshold: self.config.stability_threshold,
            snapshot_count: self.stability_snapshots.len(),
            previous_snapshot: previous_snapshot.clone(),
            match_count,
        };

        let decision = self.termination_policy.evaluate(&snap);

        // Push the current snapshot onto the history for the next tick
        // (only when we actually have enough hypotheses to compare).
        if !top_pairs.is_empty() {
            self.stability_snapshots.push(top_pairs);
        }

        match decision {
            TerminationDecision::Continue => Ok(None),
            TerminationDecision::Terminate { reason } => Ok(Some(reason)),
        }
    }

    /// Cancel pending tasks, generate final report, write to DB + file.
    async fn finalize(&self) -> Result<()> {
        // Cancel before running metareview — but cancel again after, in
        // case a follow-up task enqueued itself between our cancel and
        // the metareview call.
        if let Err(e) = self.queue.cancel_pending(&self.session_id).await {
            tracing::warn!(error = %e, "cancel_pending failed during finalize");
        }

        let hyp_repo = HypothesisRepo::new(self.memory.db_arc());
        let top = hyp_repo.top_n(&self.session_id, 10).await?;

        let mut top_block = String::new();
        for (i, h) in top.iter().enumerate() {
            let summary = if let Some(sid) = h.semantic_id {
                let mut rows = self
                    .memory
                    .db()
                    .conn()
                    .query(
                        "SELECT summary FROM semantic_memories WHERE id = ?1",
                        [sid],
                    )
                    .await?;
                rows.next()
                    .await?
                    .and_then(|r| r.get::<String>(0).ok())
                    .unwrap_or_else(|| "(no summary)".into())
            } else {
                "(no summary)".into()
            };
            top_block.push_str(&format!("{}. [Elo {:.0}] {}\n", i + 1, h.elo, summary));
        }

        let mut ctx = PromptContext::new();
        ctx.set("goal", &self.goal);
        ctx.set("preferences", &self.preferences);
        ctx.set("system_feedback", "(none)");
        ctx.set("top_hypotheses_block", &top_block);
        let rendered = self.prompts.render(AgentMode::MetaReviewFinal, &ctx)?;

        // Reuse the per-session Runner cache so we don't spawn a
        // second Claude subprocess for the metareview role. The cache
        // is shared with `RunAgentTool`; if a metareview has already
        // run during the session (idle injection), the cached Runner
        // is reused. On a cache miss we build a fresh Runner with the
        // same registry — same path `RunAgentTool` takes.
        let agent = AGENTS.iter().find(|a| a.name == "metareview").unwrap();
        let runner_arc = self.session_runners.get_or_build(
            &self.session_id,
            agent.name,
            self.memory.clone(),
            self.registry.clone(),
            crate::runner::RunnerConfig::default(),
        );
        let mut runner = runner_arc.lock().await;
        match runner.turn(agent, &rendered).await {
            Ok(outcome) => {
                let report = outcome.cleaned_text;
                self.repo
                    .mark_done_with_report(
                        &self.session_id,
                        &report,
                        &chrono::Utc::now().to_rfc3339(),
                    )
                    .await?;
                if let Err(e) = std::fs::write("report.md", &report) {
                    tracing::warn!(error = %e, "failed to write report.md; DB copy is still authoritative");
                }
                info!("final report written to report.md");
            }
            Err(e) => {
                error!(error = %e, "final overview agent failed");
                self.repo
                    .finalize(&self.session_id, &chrono::Utc::now().to_rfc3339())
                    .await?;
            }
        }

        // Second cancel pass: any task enqueued during metareview
        // (e.g. follow-up reflections triggered by RunAgentTool) gets
        // cleaned up before we signal shutdown.
        let cancelled = self
            .queue
            .cancel_pending(&self.session_id)
            .await
            .unwrap_or(0);
        if cancelled > 0 {
            info!(cancelled, "cancelled late-enqueued tasks before shutdown");
        }

        info!(
            hypotheses = top.len(),
            elapsed = ?self.start_time.elapsed(),
            "session finalized"
        );
        Ok(())
    }
}