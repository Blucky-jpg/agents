/// Tool: run an agent turn via the Runner, then enqueue follow-up tasks.
///
/// This is the bridge between the task queue and the LLM. When the
/// worker dispatches a `run_agent` task, this tool:
///   1. Creates a Runner for the session
///   2. Calls `runner.turn(agent, prompt)`
///   3. Extracts hypothesis/review IDs from dispatched markers
///   4. Enqueues follow-up tasks based on the agent that just ran
///
/// The tool closes over the TaskQueue and Prompts so it can enqueue
/// follow-ups without caller involvement.

use std::sync::Arc;

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::agents::AGENTS;
use crate::hypothesis::HypothesisRepo;
use crate::memory::Memory;
use crate::prompts::{AgentMode, Prompts, PromptContext};
use crate::queue::{EnqueueRequest, TaskQueue};
use crate::runner::{Runner, RunnerConfig};
use crate::tool::{Tool, ToolCtx, ToolOutput};
use crate::tournament::TournamentRepo;

pub struct RunAgentTool {
    queue: TaskQueue,
    prompts: Arc<Prompts>,
    registry: Arc<crate::registry::ToolRegistry>,
    runner_config: RunnerConfig,
}

impl RunAgentTool {
    pub fn new(
        queue: TaskQueue,
        prompts: Arc<Prompts>,
        registry: Arc<crate::registry::ToolRegistry>,
        runner_config: RunnerConfig,
    ) -> Self {
        Self {
            queue,
            prompts,
            registry,
            runner_config,
        }
    }
}

impl std::fmt::Debug for RunAgentTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunAgentTool").finish()
    }
}

#[async_trait]
impl Tool for RunAgentTool {
    fn name(&self) -> &str {
        "run_agent"
    }

    fn description(&self) -> String {
        "Execute an agent turn: call the LLM with the agent's system prompt, \
         parse tool markers from the response, dispatch them, and enqueue \
         follow-up tasks for the next pipeline stage."
            .to_string()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "string",
                    "description": "Agent name (supervisor, generation, reflection, ranking, evolution, metareview)."
                },
                "mode": {
                    "type": "string",
                    "description": "Prompt mode filename (e.g. generation_literature, reflection_review)."
                },
                "prompt": {
                    "type": "string",
                    "description": "The rendered user prompt for this turn."
                },
                "hypothesis_id": {
                    "type": "integer",
                    "description": "Optional hypothesis ID for review/ranking tasks."
                },
                "goal": {
                    "type": "string",
                    "description": "The research goal."
                },
                "preferences": {
                    "type": "string",
                    "description": "User preferences/criteria."
                }
            },
            "required": ["agent", "prompt"]
        })
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let agent_name = args
            .get("agent")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("run_agent: missing 'agent'"))?;
        let user_prompt = args
            .get("prompt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("run_agent: missing 'prompt'"))?;

        // If the prompt is empty, try to build it from context.
        let user_prompt = if user_prompt.is_empty() {
            let goal = args.get("goal").and_then(|v| v.as_str()).unwrap_or("");
            let preferences = args.get("preferences").and_then(|v| v.as_str()).unwrap_or("");
            let hyp_id = args.get("hypothesis_id").and_then(|v| v.as_i64());
            build_prompt_for_agent(
                &ctx.memory,
                &self.prompts,
                agent_name,
                goal,
                preferences,
                hyp_id,
                &ctx.run_id,
            )
            .await
            .unwrap_or_else(|e| {
                warn!(error = %e, "failed to build prompt, using empty");
                String::new()
            })
        } else {
            user_prompt.to_string()
        };

        // Resolve the agent.
        let agent = AGENTS
            .iter()
            .find(|a| a.name == agent_name)
            .ok_or_else(|| anyhow::anyhow!("unknown agent: {agent_name}"))?;

        // Create a Runner for this session.
        let mut runner = Runner::with_registry(
            ctx.memory.clone(),
            self.registry.clone(),
            &ctx.run_id,
            self.runner_config.clone(),
        );

        // Run the agent turn.
        let outcome = runner
            .turn(agent, &user_prompt)
            .await
            .context("agent turn failed")?;

        // Extract IDs from dispatched markers.
        // Track which memory ids were created during this turn, so we can
        // pick up hypotheses even if the LLM used save_semantic instead of
        // record_hypothesis.
        let mut hypothesis_ids: Vec<i64> = Vec::new();
        let mut review_ids: Vec<i64> = Vec::new();
        let mut saved_semantic_ids: Vec<i64> = Vec::new();
        for marker in outcome.markers.iter() {
            match marker.op.as_str() {
                "record_hypothesis" => {
                    if let Some(id) = find_latest_hypothesis_id(&ctx.memory, &ctx.run_id).await? {
                        hypothesis_ids.push(id);
                    }
                }
                "save_semantic" => {
                    // LLM may have used save_semantic instead of record_hypothesis.
                    // Track it so we can check if it's a hypothesis below.
                    if let Some(id) = marker.payload.get("id").and_then(|v| v.as_i64()) {
                        saved_semantic_ids.push(id);
                    }
                }
                "record_review" => {
                    if let Some(id) = marker.payload.get("hypothesis_id").and_then(|v| v.as_i64()) {
                        review_ids.push(id);
                    }
                }
                _ => {}
            }
        }
        // If the agent just ran generation, look up any semantic memories
        // created with scope=hypothesis or scope=insight that aren't yet
        // linked to a hypothesis row.
        if agent_name == "generation" && hypothesis_ids.is_empty() && !saved_semantic_ids.is_empty() {
            for sid in saved_semantic_ids {
                if let Some(hid) = ensure_hypothesis_for_semantic(&ctx.memory, sid, &ctx.run_id).await? {
                    hypothesis_ids.push(hid);
                }
            }
        }

        // Enqueue follow-up tasks based on which agent just ran.
        let goal = args.get("goal").and_then(|v| v.as_str()).unwrap_or("");
        let preferences = args.get("preferences").and_then(|v| v.as_str()).unwrap_or("");
        enqueue_follow_ups(
            &ctx.memory,
            &self.queue,
            &self.prompts,
            agent_name,
            &ctx.run_id,
            goal,
            preferences,
            &hypothesis_ids,
            &review_ids,
        )
        .await?;

        Ok(json!({
            "cleaned_text": outcome.cleaned_text,
            "markers_dispatched": outcome.markers.len(),
            "hypothesis_ids": hypothesis_ids,
            "review_ids": review_ids,
        }))
    }
}

// ── Context helpers ────────────────────────────────────────────────

/// Build a prompt for an agent based on the task type.
async fn build_prompt_for_agent(
    memory: &Memory,
    prompts: &Prompts,
    agent_name: &str,
    goal: &str,
    preferences: &str,
    hypothesis_id: Option<i64>,
    session_id: &str,
) -> Result<String> {
    match agent_name {
        "reflection" => {
            let hyp_id = hypothesis_id.ok_or_else(|| anyhow::anyhow!("reflection needs hypothesis_id"))?;
            build_review_prompt(memory, prompts, goal, preferences, hyp_id).await
        }
        "ranking" => {
            // Pick two hypotheses for pairwise comparison.
            let repo = HypothesisRepo::new(memory.db_arc());
            let needs = repo.needs_matches(session_id, 3, 2).await?;
            if needs.len() >= 2 {
                build_pairwise_prompt(memory, prompts, goal, preferences, needs[0].id, needs[1].id, session_id).await
            } else {
                Ok(String::new())
            }
        }
        "evolution" => {
            let repo = HypothesisRepo::new(memory.db_arc());
            let top = repo.top_n(session_id, 2).await?;
            if top.len() >= 2 {
                build_evolution_prompt(memory, prompts, goal, preferences, top[0].id, top[1].id, session_id).await
            } else {
                Ok(String::new())
            }
        }
        "generation" => {
            build_generation_prompt(memory, prompts, goal, preferences, session_id).await
        }
        "metareview" => {
            build_metareview_prompt(memory, prompts, goal, preferences, session_id).await
        }
        _ => Ok(String::new()),
    }
}

/// Build a reflection review prompt with actual hypothesis text.
async fn build_review_prompt(
    memory: &Memory,
    prompts: &Prompts,
    goal: &str,
    preferences: &str,
    hypothesis_id: i64,
) -> Result<String> {
    let hyp_text = fetch_hypothesis_text(memory, hypothesis_id).await?;
    let mut ctx = PromptContext::new();
    ctx.set("goal", goal);
    ctx.set("preferences", preferences);
    ctx.set("hypothesis_id", &hypothesis_id.to_string());
    ctx.set("hypothesis_text", &hyp_text);
    ctx.set("articles_block", "(no literature retrieved)");
    prompts
        .render(AgentMode::ReflectionReview, &ctx)
        .context("rendering reflection_review prompt")
}

/// Build a pairwise ranking prompt with two hypotheses and their reviews.
async fn build_pairwise_prompt(
    memory: &Memory,
    prompts: &Prompts,
    goal: &str,
    preferences: &str,
    hyp_a: i64,
    hyp_b: i64,
    session_id: &str,
) -> Result<String> {
    let text_a = fetch_hypothesis_text(memory, hyp_a).await?;
    let text_b = fetch_hypothesis_text(memory, hyp_b).await?;
    let review_a = fetch_latest_review(memory, hyp_a, session_id).await;
    let review_b = fetch_latest_review(memory, hyp_b, session_id).await;
    let mut ctx = PromptContext::new();
    ctx.set("goal", goal);
    ctx.set("preferences", preferences);
    ctx.set("hypothesis_1_id", &hyp_a.to_string());
    ctx.set("hypothesis_1", &text_a);
    ctx.set("hypothesis_2_id", &hyp_b.to_string());
    ctx.set("hypothesis_2", &text_b);
    ctx.set("review_1", &review_a);
    ctx.set("review_2", &review_b);
    ctx.set("notes", "");
    prompts
        .render(AgentMode::RankingPairwise, &ctx)
        .context("rendering ranking_pairwise prompt")
}

/// Build an evolution combine prompt with two hypotheses and their reviews.
async fn build_evolution_prompt(
    memory: &Memory,
    prompts: &Prompts,
    goal: &str,
    preferences: &str,
    hyp_a: i64,
    hyp_b: i64,
    session_id: &str,
) -> Result<String> {
    let text_a = fetch_hypothesis_text(memory, hyp_a).await?;
    let text_b = fetch_hypothesis_text(memory, hyp_b).await?;
    let review_a = fetch_latest_review(memory, hyp_a, session_id).await;
    let review_b = fetch_latest_review(memory, hyp_b, session_id).await;
    let mut ctx = PromptContext::new();
    ctx.set("goal", goal);
    ctx.set("preferences", preferences);
    ctx.set("hypothesis_a_id", &hyp_a.to_string());
    ctx.set("hypothesis_a", &text_a);
    ctx.set("hypothesis_b_id", &hyp_b.to_string());
    ctx.set("hypothesis_b", &text_b);
    ctx.set("review_a", &review_a);
    ctx.set("review_b", &review_b);
    prompts
        .render(AgentMode::EvolutionCombine, &ctx)
        .context("rendering evolution_combine prompt")
}

/// Build a generation prompt with system feedback injected.
async fn build_generation_prompt(
    memory: &Memory,
    prompts: &Prompts,
    goal: &str,
    preferences: &str,
    _session_id: &str,
) -> Result<String> {
    let feedback = memory.recent_system_feedback(5).await.join("\n---\n");
    let articles = format!(
        "(no external literature available; use your training knowledge)\n\n{}",
        if feedback.is_empty() {
            String::new()
        } else {
            format!("## System Feedback\n{feedback}")
        }
    );
    let mut ctx = PromptContext::new();
    ctx.set("goal", goal);
    ctx.set("preferences", preferences);
    ctx.set("articles_with_reasoning", &articles);
    ctx.set("source_hypothesis", "");
    ctx.set("instructions", "");
    prompts
        .render(AgentMode::GenerationLiterature, &ctx)
        .context("rendering generation_literature prompt")
}

/// Build a metareview system feedback prompt with recent tournament data.
async fn build_metareview_prompt(
    memory: &Memory,
    prompts: &Prompts,
    goal: &str,
    preferences: &str,
    session_id: &str,
) -> Result<String> {
    let tour_repo = TournamentRepo::new(memory.db_arc());
    let rationales = tour_repo.recent_rationales(session_id, 50).await.unwrap_or_default();
    let reviews_text = if rationales.is_empty() {
        "(no tournament data yet)".to_string()
    } else {
        rationales.join("\n---\n")
    };
    let mut ctx = PromptContext::new();
    ctx.set("goal", goal);
    ctx.set("preferences", preferences);
    ctx.set("reviews_overview", &reviews_text);
    ctx.set("transcript", "");
    prompts
        .render(AgentMode::MetaReviewSystem, &ctx)
        .context("rendering metareview_system prompt")
}

// ── DB helpers ─────────────────────────────────────────────────────

/// Fetch the summary text of a hypothesis from semantic_memories.
async fn fetch_hypothesis_text(memory: &Memory, hypothesis_id: i64) -> Result<String> {
    let repo = HypothesisRepo::new(memory.db_arc());
    let hyp = repo
        .get(hypothesis_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("hypothesis {hypothesis_id} not found"))?;
    if let Some(sid) = hyp.semantic_id {
        let mut rows = memory
            .db()
            .conn()
            .query(
                "SELECT COALESCE(details_json, summary) FROM semantic_memories WHERE id = ?1",
                [sid],
            )
            .await?;
        if let Some(row) = rows.next().await? {
            let text: String = row.get(0)?;
            return Ok(text);
        }
    }
    Ok("(hypothesis text not available)".to_string())
}

/// Fetch the latest review for a hypothesis.
async fn fetch_latest_review(memory: &Memory, hypothesis_id: i64, session_id: &str) -> String {
    // Reviews are semantic_memories with scope='review' whose details reference the hypothesis.
    let result = memory
        .db()
        .conn()
        .query(
            "SELECT summary, details_json FROM semantic_memories
             WHERE run_id = ?1 AND scope = 'review'
             ORDER BY id DESC LIMIT 10",
            [session_id],
        )
        .await;
    let mut rows = match result {
        Ok(r) => r,
        Err(_) => return "(no review available)".to_string(),
    };
    while let Some(row) = rows.next().await.unwrap_or(None) {
        let details: Option<String> = row.get(1).unwrap_or(None);
        if let Some(d) = &details {
            if d.contains(&hypothesis_id.to_string()) {
                let summary: String = row.get(0).unwrap_or_default();
                return summary;
            }
        }
    }
    "(no review available)".to_string()
}

/// Find the most recently created hypothesis ID for a session.
async fn find_latest_hypothesis_id(memory: &Memory, session_id: &str) -> Result<Option<i64>> {
    let repo = HypothesisRepo::new(memory.db_arc());
    let hyps = repo.list_by_session(session_id).await?;
    Ok(hyps.last().map(|h| h.id))
}

/// If a semantic memory was saved with scope=hypothesis or scope=insight
/// but no hypothesis row exists yet, create one linking them.
async fn ensure_hypothesis_for_semantic(
    memory: &Memory,
    semantic_id: i64,
    session_id: &str,
) -> Result<Option<i64>> {
    // Check the scope of the semantic memory.
    let scope: Option<String> = memory
        .db()
        .conn()
        .query("SELECT scope FROM semantic_memories WHERE id = ?1", [semantic_id])
        .await?
        .next()
        .await?
        .and_then(|r| r.get(0).ok());
    let scope = match scope.as_deref() {
        Some("hypothesis") | Some("insight") => scope.unwrap(),
        _ => return Ok(None),
    };
    // Check if a hypothesis already links this semantic_id.
    let repo = HypothesisRepo::new(memory.db_arc());
    let hyps = repo.list_by_session(session_id).await?;
    if let Some(existing) = hyps.iter().find(|h| h.semantic_id == Some(semantic_id)) {
        return Ok(Some(existing.id));
    }
    // Promote the scope to "hypothesis" and create the row.
    memory
        .db()
        .conn()
        .execute(
            "UPDATE semantic_memories SET scope = 'hypothesis' WHERE id = ?1",
            [semantic_id],
        )
        .await?;
    let hyp_id = repo.insert(session_id, Some(semantic_id), &[], 1200.0).await?;
    info!(
        semantic_id,
        hyp_id, scope, "promoted semantic memory to hypothesis"
    );
    Ok(Some(hyp_id))
}

// ── Follow-up enqueueing ───────────────────────────────────────────

/// Enqueue follow-up tasks based on which agent completed.
async fn enqueue_follow_ups(
    memory: &Memory,
    queue: &TaskQueue,
    prompts: &Prompts,
    agent_name: &str,
    session_id: &str,
    goal: &str,
    preferences: &str,
    hypothesis_ids: &[i64],
    _review_ids: &[i64],
) -> Result<()> {
    match agent_name {
        "generation" => {
            // After generation: review each new hypothesis with full context.
            for &hyp_id in hypothesis_ids {
                let rendered = build_review_prompt(memory, prompts, goal, preferences, hyp_id)
                    .await
                    .unwrap_or_default();
                queue
                    .enqueue(EnqueueRequest {
                        session_id: session_id.to_string(),
                        agent: "reflection".to_string(),
                        action: "run_agent".to_string(),
                        payload: json!({
                            "agent": "reflection",
                            "mode": "reflection_review",
                            "prompt": rendered,
                            "hypothesis_id": hyp_id,
                            "goal": goal,
                            "preferences": preferences,
                        }),
                        priority: 100,
                        max_attempts: 3,
                    })
                    .await?;
                info!(hyp_id, "enqueued reflection/ReviewHypothesis");
            }
        }
        "reflection" => {
            // After review: add to tournament (ranking picks hypotheses itself).
            queue
                .enqueue(EnqueueRequest {
                    session_id: session_id.to_string(),
                    agent: "ranking".to_string(),
                    action: "run_agent".to_string(),
                    payload: json!({
                        "agent": "ranking",
                        "mode": "ranking_pairwise",
                        "prompt": "",
                        "goal": goal,
                        "preferences": preferences,
                    }),
                    priority: 100,
                    max_attempts: 3,
                })
                .await?;
            info!("enqueued ranking/RunTournamentBatch");
        }
        "evolution" => {
            // After evolution: review new hypotheses.
            for &hyp_id in hypothesis_ids {
                let rendered = build_review_prompt(memory, prompts, goal, preferences, hyp_id)
                    .await
                    .unwrap_or_default();
                queue
                    .enqueue(EnqueueRequest {
                        session_id: session_id.to_string(),
                        agent: "reflection".to_string(),
                        action: "run_agent".to_string(),
                        payload: json!({
                            "agent": "reflection",
                            "mode": "reflection_review",
                            "prompt": rendered,
                            "hypothesis_id": hyp_id,
                            "goal": goal,
                            "preferences": preferences,
                        }),
                        priority: 100,
                        max_attempts: 3,
                    })
                    .await?;
                info!(hyp_id, "enqueued reflection for evolved hypothesis");
            }
        }
        "ranking" | "supervisor" | "metareview" => {
            // No automatic follow-ups; supervisor handles idle injection.
        }
        _ => {}
    }
    Ok(())
}
