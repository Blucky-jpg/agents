//! Workload patterns for the soak runner.
//!
//! Seven patterns rotated by the runner. Each pattern exercises a
//! different surface of the memory subsystem so 24/7 coverage doesn't
//! devolve into a single-code-path stress test.
//!
//! **Real-world task resemblance:** patterns A/B/C mirror what the
//! 7-agent pipeline actually does (multi-turn research session,
//! cross-session recall, behavior self-critique). D/E/F are the
//! pathological cases (high churn, scale stress, concurrent writers)
//! that the production load occasionally hits. G is exhaustive
//! edge-input fuzzing.
//!
//! Each pattern returns `WorkloadOutcome` with metrics the runner
//! records. Patterns never panic — they catch their own internal
//! errors and surface them as `WorkloadOutcome::errored`.

use std::time::Instant;

use co_scientist::memory::{ContextLimits, Memory, ObservationKind};

use super::fixture::{edge_summaries, lexical_match, observation, paraphrase, query_for_topic, research_session, AGENTS, TOPICS};

pub enum WorkloadKind {
    /// Single agent reads + writes within one session (peek→observe flow).
    SingleAgentRecall,
    /// Full research session: supervisor + 3 hypotheses + experiment + reflection.
    ResearchSession,
    /// Save in run-A, query in run-B (no run filter).
    CrossSessionRecall,
    /// Save/observe/save/observe — exercises idempotency + near-dup at scale.
    HighChurn,
    /// Insert 1k memories, run 100 searches — exercises scale path.
    ScaleStress,
    /// 16 concurrent agents writing the same scope simultaneously.
    ConcurrentWriters,
    /// Exhaustive edge inputs: empty / unicode / huge / SQL-like / etc.
    EdgeInputFuzz,
}

// All variants are unit-only, so Copy is free. The runner uses
// `WorkloadKind::all()[idx]` and needs to copy out of the static array.
impl Copy for WorkloadKind {}
impl Clone for WorkloadKind {
    fn clone(&self) -> Self {
        *self
    }
}

impl WorkloadKind {
    pub fn all() -> [WorkloadKind; 7] {
        [
            WorkloadKind::SingleAgentRecall,
            WorkloadKind::ResearchSession,
            WorkloadKind::CrossSessionRecall,
            WorkloadKind::HighChurn,
            WorkloadKind::ScaleStress,
            WorkloadKind::ConcurrentWriters,
            WorkloadKind::EdgeInputFuzz,
        ]
    }
}

#[derive(Default)]
pub struct WorkloadOutcome {
    pub saves: usize,
    pub searches: usize,
    pub observations_fetched: usize,
    pub contexts_rendered: usize,
    pub mismatches: usize, // recall@K failures (top result didn't match ground truth)
    pub errored: bool,
    pub error_label: &'static str,
}

/// Run one workload. Caller is responsible for panic-catch via the runner.
pub async fn run(kind: WorkloadKind, memory: &Memory, iter: u64) -> WorkloadOutcome {
    let started = Instant::now();
    let mut out = WorkloadOutcome::default();
    let res = match kind {
        WorkloadKind::SingleAgentRecall => run_single_agent(memory, iter, &mut out).await,
        WorkloadKind::ResearchSession => run_research_session(memory, iter, &mut out).await,
        WorkloadKind::CrossSessionRecall => run_cross_session(memory, iter, &mut out).await,
        WorkloadKind::HighChurn => run_high_churn(memory, iter, &mut out).await,
        WorkloadKind::ScaleStress => run_scale_stress(memory, iter, &mut out).await,
        WorkloadKind::ConcurrentWriters => run_concurrent(memory, iter, &mut out).await,
        WorkloadKind::EdgeInputFuzz => run_edge_fuzz(memory, iter, &mut out).await,
    };
    if let Err(e) = res {
        out.errored = true;
        out.error_label = label_for(&kind);
        super::telemetry::soft_fail(
            "workload.error",
            format!("kind={} iter={iter} err={e} elapsed={:?}", label_for(&kind), started.elapsed()),
        );
    }
    out
}

fn label_for(k: &WorkloadKind) -> &'static str {
    match k {
        WorkloadKind::SingleAgentRecall => "single_agent_recall",
        WorkloadKind::ResearchSession => "research_session",
        WorkloadKind::CrossSessionRecall => "cross_session_recall",
        WorkloadKind::HighChurn => "high_churn",
        WorkloadKind::ScaleStress => "scale_stress",
        WorkloadKind::ConcurrentWriters => "concurrent_writers",
        WorkloadKind::EdgeInputFuzz => "edge_input_fuzz",
    }
}

// ---- individual workloads ----

async fn run_single_agent(memory: &Memory, iter: u64, out: &mut WorkloadOutcome) -> anyhow::Result<()> {
    let run_id = format!("single-{iter}");
    let agent = AGENTS[(iter as usize) % AGENTS.len()];
    // Write a topic-tagged observation.
    let topic = TOPICS[(iter as usize) % TOPICS.len()];
    let obs = observation(iter as usize);
    let _id = memory.save_semantic(&run_id, Some(agent), obs.scope, &obs.summary, Some(obs.details.clone())).await?;
    out.saves += 1;
    // Peek it back.
    let query = query_for_topic(topic);
    let peeked = memory.peek_context(agent, &query, 5).await?;
    out.searches += 1;
    if !peeked.iter().any(|p| lexical_match(&query, &p.summary)) {
        out.mismatches += 1;
        super::telemetry::soft_fail(
            "single_agent.no_topic_match",
            format!("topic={topic} query={query} peeked_count={}", peeked.len()),
        );
    }
    // Render full context.
    let ctx = memory
        .get_context(&run_id, agent, &query, ContextLimits { events: 5, semantic: 5, behavior: 3, max_tokens: 0, full_count: 3 })
        .await?;
    out.contexts_rendered += 1;
    if ctx.rendered.is_empty() {
        super::telemetry::soft_fail("single_agent.empty_render", "ctx.rendered was empty");
    }
    Ok(())
}

async fn run_research_session(memory: &Memory, iter: u64, out: &mut WorkloadOutcome) -> anyhow::Result<()> {
    let run_id = format!("session-{iter}");
    let session = research_session(iter as usize);
    for obs in &session {
        memory
            .save_semantic(&run_id, Some(obs.agent), obs.scope, &obs.summary, Some(obs.details.clone()))
            .await?;
        out.saves += 1;
    }
    // Now query from a downstream agent (metareview) and check that
    // at least one of the 7 session entries surfaces.
    let metareview = "metareview";
    let query = query_for_topic(session[0].topic);
    let peeked = memory.peek_context(metareview, &query, 10).await?;
    out.searches += 1;
    let session_summaries: Vec<&str> = session.iter().map(|o| o.summary.as_str()).collect();
    let any_hit = peeked.iter().any(|p| session_summaries.iter().any(|s| s.contains(&p.summary) || p.summary.contains(s)));
    if !any_hit && !peeked.is_empty() {
        // Soft fail — peek may surface older cross-session memories, which is fine.
        // Only fail if NO match was found AND we wrote 7 entries.
        super::telemetry::soft_fail(
            "research_session.no_session_match",
            format!("peeked={} session_entries={}", peeked.len(), session.len()),
        );
        out.mismatches += 1;
    }
    Ok(())
}

async fn run_cross_session(memory: &Memory, iter: u64, out: &mut WorkloadOutcome) -> anyhow::Result<()> {
    let run_a = format!("cross-A-{iter}");
    let run_b = format!("cross-B-{iter}");
    let topic = TOPICS[(iter as usize) % TOPICS.len()];
    let obs = observation(iter as usize);
    memory.save_semantic(&run_a, Some(obs.agent), obs.scope, &obs.summary, Some(obs.details.clone())).await?;
    out.saves += 1;
    // Query from run_b — the retrieval should find run_a's save because
    // the topic matches lexically. (run_id filter is a UI concern; the
    // memory API itself is run-agnostic for peek_context.)
    let query = query_for_topic(topic);
    let peeked = memory.peek_context("reflection", &query, 10).await?;
    out.searches += 1;
    let hit = peeked.iter().any(|p| lexical_match(&query, &p.summary));
    if !hit {
        out.mismatches += 1;
        super::telemetry::soft_fail(
            "cross_session.no_recall",
            format!("topic={topic} peeked_count={}", peeked.len()),
        );
    }
    // Run B can also do its own writes; we just don't require them.
    let _ = memory.save_semantic(&run_b, Some("reflection"), "plan", "cross-session round-trip", None).await?;
    out.saves += 1;
    Ok(())
}

async fn run_high_churn(memory: &Memory, iter: u64, out: &mut WorkloadOutcome) -> anyhow::Result<()> {
    let run_id = format!("churn-{iter}");
    let obs = observation(iter as usize);
    let mut ids = Vec::new();
    // Save the same observation 10 times — must dedupe to 1 row.
    for _ in 0..10 {
        let id = memory.save_semantic(&run_id, Some(obs.agent), obs.scope, &obs.summary, Some(obs.details.clone())).await?;
        ids.push(id);
        out.saves += 1;
    }
    let unique: std::collections::HashSet<i64> = ids.iter().copied().collect();
    if unique.len() != 1 {
        super::telemetry::soft_fail(
            "high_churn.dedup_failed",
            format!("expected 1 unique id, got {} (ids={ids:?})", unique.len()),
        );
    }
    // Now save a near-duplicate paraphrase — should also dedupe (threshold 0.92).
    let para = paraphrase(&obs.summary);
    let id_para = memory.save_semantic(&run_id, Some(obs.agent), obs.scope, &para, None).await?;
    if !unique.contains(&id_para) {
        // Note: near-dup detection uses the full embedding; paraphrase
        // may not cross the 0.92 threshold depending on stem overlap.
        // We only flag if it duplicates a different memory.
        super::telemetry::soft_fail(
            "high_churn.paraphrase_new_id",
            format!("expected dedupe or same id; got new id {id_para} vs original {ids:?}"),
        );
    }
    // Then a clearly different observation — should get a new id.
    let other = observation((iter as usize) + 1000);
    let id_other = memory.save_semantic(&run_id, Some(other.agent), other.scope, &other.summary, Some(other.details.clone())).await?;
    if unique.contains(&id_other) {
        super::telemetry::soft_fail(
            "high_churn.different_collision",
            format!("different observation got the same id {id_other} as the dedup set"),
        );
    }
    out.searches += 1;
    Ok(())
}

async fn run_scale_stress(memory: &Memory, iter: u64, out: &mut WorkloadOutcome) -> anyhow::Result<()> {
    let run_id = format!("scale-{iter}");
    // Seed 200 memories (cap so the soak stays bounded).
    for i in 0..200 {
        let obs = observation(i);
        memory.save_semantic(&run_id, Some(obs.agent), obs.scope, &obs.summary, Some(obs.details.clone())).await?;
        out.saves += 1;
    }
    // Run 30 searches across random topics + queries.
    let n_searches = 30usize;
    let mut total_results = 0usize;
    for i in 0..n_searches {
        let topic = TOPICS[(i + iter as usize) % TOPICS.len()];
        let query = query_for_topic(topic);
        let peeked = memory.peek_context(AGENTS[i % AGENTS.len()], &query, 10).await?;
        out.searches += 1;
        total_results += peeked.len();
        if peeked.is_empty() {
            super::telemetry::soft_fail(
                "scale.empty_result",
                format!("topic={topic} query={query} iter={iter}"),
            );
        }
    }
    if total_results < n_searches {
        // Very loose sanity check: at least one result per search on average.
        super::telemetry::soft_fail(
            "scale.low_yield",
            format!("total_results={total_results} searches={n_searches} iter={iter}"),
        );
    }
    Ok(())
}

async fn run_concurrent(memory: &Memory, iter: u64, out: &mut WorkloadOutcome) -> anyhow::Result<()> {
    use futures::future::join_all;
    let run_id = format!("concurrent-{iter}");
    let n = 16usize;
    let mut handles = Vec::with_capacity(n);
    for i in 0..n {
        let m = memory.clone();
        let r = run_id.clone();
        handles.push(tokio::spawn(async move {
            // Each "agent" saves 3 observations with the same scope, different summaries.
            for j in 0..3 {
                let obs = observation((iter as usize) + i * 7 + j);
                m.save_semantic(&r, Some(AGENTS[i % AGENTS.len()]), obs.scope, &obs.summary, Some(obs.details.clone())).await?;
            }
            anyhow::Ok(())
        }));
    }
    let results = join_all(handles).await;
    for r in results {
        match r {
            Ok(Ok(())) => out.saves += 3,
            Ok(Err(e)) => {
                super::telemetry::soft_fail("concurrent.task_err", format!("{e}"));
            }
            Err(join_err) => {
                super::telemetry::soft_fail("concurrent.join_err", format!("{join_err}"));
            }
        }
    }
    // Read-back: search should still work after concurrent writes.
    let peeked = memory.peek_context(AGENTS[0], &query_for_topic(TOPICS[0]), 20).await?;
    out.searches += 1;
    if peeked.is_empty() {
        super::telemetry::soft_fail("concurrent.empty_after_writes", "peek empty after concurrent inserts");
    }
    Ok(())
}

async fn run_edge_fuzz(memory: &Memory, iter: u64, out: &mut WorkloadOutcome) -> anyhow::Result<()> {
    let run_id = format!("edge-{iter}");
    let cases = edge_summaries();
    for (label, summary) in &cases {
        // Try to save each edge case. Some will succeed; some may be
        // rejected by validation. We accept either — the assertion is
        // that no case CRASHES the memory layer.
        let result = memory
            .save_semantic(&run_id, Some("experiment"), "insight", summary, None)
            .await;
        match result {
            Ok(_id) => out.saves += 1,
            Err(e) => {
                // Edge case was rejected — that's fine, log it for visibility.
                super::telemetry::soft_fail(
                    "edge.rejected",
                    format!("label={label} err={e}"),
                );
            }
        }
    }
    // Empty query must not crash.
    let _ = memory.peek_context("experiment", "", 5).await;
    out.searches += 1;
    // Stop-word-only query must not crash.
    let _ = memory.peek_context("experiment", "the a an of", 5).await;
    out.searches += 1;
    // get_observation with a definitely-nonexistent id must not crash.
    let _ = memory.get_observation(ObservationKind::Semantic, -1).await;
    out.observations_fetched += 1;
    Ok(())
}