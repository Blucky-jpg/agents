/// Tool: run an agent turn via the Runner, then enqueue follow-up tasks.
///
/// This is the bridge between the task queue and the LLM. When the
/// worker dispatches a `run_agent` task, this tool:
///   1. Acquires (or lazily builds) a Runner for `(session_id, agent)`
///      from the [`SessionRunners`] cache
///   2. Calls `runner.turn(agent, prompt)`
///   3. Extracts hypothesis/review IDs from dispatched markers
///   4. Enqueues follow-up tasks based on the agent that just ran
///
/// The tool closes over the TaskQueue and Prompts so it can enqueue
/// follow-ups without caller involvement.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::Mutex as AsyncMutex;

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
use crate::tournament::matches::TournamentRepo;

/// Maximum number of within-turn retries for the ranking agent when
/// the first attempt fails to dispatch `record_tournament_match`. The
/// cap exists because the LLM can occasionally fixate on a wrong
/// format forever; 2 retries (3 total attempts) is enough headroom
/// for the self-correction block to land while bounding the worst
/// case at 3 × LLM latency per ranking task.
const RANKING_MAX_RETRIES: u32 = 2;

/// Per-session cache of [`Runner`] handles, keyed by `(session_id, agent_name)`.
///
/// A long supervisor session that runs 50 generation / reflection /
/// ranking tasks used to spawn 50 fresh `Runner`s — each one re-paying
/// the `claude` subprocess connect cost and rebuilding the
/// `PromptContextCache`. This cache amortizes both costs: a session's
/// first task on a given agent builds the Runner; every subsequent
/// task reuses it.
///
/// Concurrency: the outer `Mutex<HashMap>` is `std::sync::Mutex` —
/// short-held on `get_or_build`. The inner per-Runner lock is
/// `tokio::sync::Mutex` because the lock must be held across the
/// async `runner.turn(...)` call. Worker pool concurrency is bounded
/// (default `concurrency = 4`), and each `turn` is a serial await on
/// the LLM, so lock contention is bounded by LLM latency, not by
/// the map. If we ever shard workers across many machines, the
/// per-session map needs a lock-free redesign — see the module-level
/// note for that future.
///
/// Drop semantics: when the `SessionRunners` is dropped (e.g. on
/// shutdown), any in-flight Runners are dropped along with it, which
/// drops their `ClaudeHandle` and terminates the child subprocesses
/// via `kill_on_drop`. There is no explicit cleanup pass.
pub struct SessionRunners {
    inner: Mutex<HashMap<(String, String), Arc<AsyncMutex<Runner>>>>,
}

impl Default for SessionRunners {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionRunners {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Get a clone of the Arc-wrapped Runner for `(session_id, agent_name)`,
    /// building a new one on a miss. The caller holds the Arc and
    /// locks the inner `tokio::sync::Mutex<Runner>` per-turn — multiple
    /// worker tasks on the same `(session, agent)` therefore serialize
    /// on the inner Runner mutex, which is fine because the underlying
    /// Claude subprocess is single-threaded.
    ///
    /// `runner_config`, `memory`, `registry` are the same arguments
    /// `Runner::with_registry` accepts. They are only used on a cache
    /// miss; cached Runners keep their original config.
    pub fn get_or_build(
        &self,
        session_id: &str,
        agent_name: &str,
        memory: Memory,
        registry: Arc<crate::registry::ToolRegistry>,
        runner_config: RunnerConfig,
    ) -> Arc<AsyncMutex<Runner>> {
        let key = (session_id.to_string(), agent_name.to_string());
        let mut map = self.inner.lock().expect("SessionRunners poisoned");
        map.entry(key)
            .or_insert_with(|| {
                Arc::new(AsyncMutex::new(Runner::with_registry(
                    memory,
                    registry,
                    session_id,
                    runner_config,
                )))
            })
            .clone()
    }

    /// Number of cached (session, agent) pairs. Exposed for tests.
    #[doc(hidden)]
    pub fn _len(&self) -> usize {
        self.inner.lock().expect("SessionRunners poisoned").len()
    }

    /// Clear all cached Runners. Intended for tests.
    #[doc(hidden)]
    pub fn _clear(&self) {
        self.inner.lock().expect("SessionRunners poisoned").clear();
    }
}

pub struct RunAgentTool {
    queue: TaskQueue,
    prompts: Arc<Prompts>,
    registry: Arc<crate::registry::ToolRegistry>,
    runner_config: RunnerConfig,
    /// Per-session Runner cache. Optional so callers who manage their
    /// own Runner lifecycle (e.g. tests) can pass `None` and fall
    /// back to the previous per-task construction path.
    session_runners: Arc<SessionRunners>,
}

impl RunAgentTool {
    pub fn new(
        queue: TaskQueue,
        prompts: Arc<Prompts>,
        registry: Arc<crate::registry::ToolRegistry>,
        runner_config: RunnerConfig,
    ) -> Self {
        Self::with_session_runners(
            queue,
            prompts,
            registry,
            runner_config,
            Arc::new(SessionRunners::new()),
        )
    }

    /// Build with an explicit SessionRunners cache. Tests and
    /// long-lived supervisors that want to share a single cache
    /// across multiple `RunAgentTool`s use this.
    pub fn with_session_runners(
        queue: TaskQueue,
        prompts: Arc<Prompts>,
        registry: Arc<crate::registry::ToolRegistry>,
        runner_config: RunnerConfig,
        session_runners: Arc<SessionRunners>,
    ) -> Self {
        Self {
            queue,
            prompts,
            registry,
            runner_config,
            session_runners,
        }
    }

    /// Read-only view of the SessionRunners cache. Lets supervisors
    /// observe or clear the cache without owning the mutator.
    pub fn session_runners(&self) -> &Arc<SessionRunners> {
        &self.session_runners
    }
}

impl std::fmt::Debug for RunAgentTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunAgentTool")
            .field("runner_config", &self.runner_config)
            .field("session_runners_count", &self.session_runners._len())
            .finish()
    }
}

impl std::fmt::Debug for SessionRunners {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionRunners")
            .field("cached_pairs", &self._len())
            .finish()
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
                &args,
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

        // Per-task prompt ↔ allowlist guard. If the queue payload
        // names a mode that doesn't belong to this agent, or a mode
        // whose prompt references tools the agent can't dispatch,
        // reject the task BEFORE any LLM call. This is the runtime
        // half of the "system smartly handles it" contract — the
        // LLM never sees a prompt that asks for tools it can't
        // call, and the supervisor never accidentally runs the
        // wrong agent against a given mode.
        if let Some(mode_str) = args.get("mode").and_then(|v| v.as_str()) {
            if !mode_str.is_empty() {
                let mode = AgentMode::from_filename(mode_str).ok_or_else(|| {
                    anyhow::anyhow!("run_agent: unknown mode `{mode_str}`")
                })?;
                let table = crate::prompt_allowlist::PromptToolTable::build()
                    .context("building prompt tool table")?;
                table
                    .validate_pair(agent_name, mode)
                    .with_context(|| {
                        format!("prompt ↔ allowlist mismatch (agent={agent_name}, mode={mode_str})")
                    })?;
            }
        }

        // Acquire (or lazily build) a per-session Runner from the
        // cache. The inner Arc<tokio::sync::Mutex<Runner>> lets us
        // re-use the ClaudeHandle and PromptContextCache across
        // tasks; the outer SessionRunners map keys on
        // (session_id, agent_name) so a single session that runs
        // generation then reflection gets two distinct Runners (each
        // owning its own subprocess), while 50 generation tasks for
        // the same session share one.
        let runner_arc = self.session_runners.get_or_build(
            &ctx.run_id,
            agent_name,
            ctx.memory.clone(),
            self.registry.clone(),
            self.runner_config.clone(),
        );
        let mut runner = runner_arc.lock().await;

        // Run the agent turn.
        let mut outcome = runner
            .turn(agent, &user_prompt)
            .await
            .context("agent turn failed")?;

        // Within-turn retry for ranking. The ranking agent is asked to
        // emit exactly one `record_tournament_match` marker per turn;
        // if the LLM hallucinates the op_name (e.g. emits `memory_op`
        // instead — observed in the 2026-06-30 "Topologie of Neural
        // nets" session) or invents the payload schema, dispatch fails
        // and zero matches get recorded for this pair. Retry up to
        // `RANKING_MAX_RETRIES` times with a self-correction message
        // that re-asserts the format and includes the original prompt
        // so the LLM still has the hypothesis IDs.
        if agent_name == "ranking" && outcome.dispatched == 0 {
            for attempt in 1..=RANKING_MAX_RETRIES {
                let retry_prompt = build_ranking_retry_prompt(&user_prompt, attempt);
                let retry_outcome = runner
                    .turn(agent, &retry_prompt)
                    .await
                    .with_context(|| format!("ranking retry turn {attempt} failed"))?;
                if retry_outcome.dispatched > 0 {
                    // Merge retry results into the original outcome so
                    // downstream code (ID extraction, follow-up enqueue)
                    // sees the recovered markers.
                    let mut merged: Vec<crate::skill::Marker> =
                        (*outcome.markers).clone();
                    merged.extend((*retry_outcome.markers).iter().cloned());
                    outcome.markers = Arc::new(merged);
                    outcome.cleaned_text = format!(
                        "{}\n{}",
                        outcome.cleaned_text, retry_outcome.cleaned_text
                    );
                    outcome.dispatched += retry_outcome.dispatched;
                    break;
                }
            }
        }

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
            &args,
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
    args: &Value,
) -> Result<String> {
    match agent_name {
        "reflection" => {
            let hyp_id = hypothesis_id.ok_or_else(|| anyhow::anyhow!("reflection needs hypothesis_id"))?;
            // The "reflection" agent has 4 modes: the original three
            // (review/observation/verification) and a new
            // `reflection_on_result` for post-experiment reflection.
            // The choice is made by the `mode` arg in the task payload.
            let mode = args.get("mode").and_then(|v| v.as_str()).unwrap_or("");
            if mode == "reflection_on_result" {
                let exp_id = args
                    .get("experiment_id")
                    .and_then(|v| v.as_i64())
                    .ok_or_else(|| {
                        anyhow::anyhow!("reflection_on_result needs experiment_id")
                    })?;
                build_reflection_on_result_prompt(
                    memory, prompts, session_id, goal, preferences, hyp_id, exp_id,
                )
                .await
            } else {
                build_review_prompt(memory, prompts, goal, preferences, hyp_id).await
            }
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
        "experiment" => {
            // Pull the experiment_id from the task payload and dispatch
            // to the right sub-prompt (design / execute / evaluate)
            // based on `mode`.
            let experiment_id = args
                .get("experiment_id")
                .and_then(|v| v.as_i64());
            let hyp_id = args
                .get("hypothesis_id")
                .and_then(|v| v.as_i64());
            let mode = args.get("mode").and_then(|v| v.as_str()).unwrap_or("");
            build_experiment_prompt(
                memory,
                prompts,
                goal,
                preferences,
                mode,
                experiment_id,
                hyp_id,
            )
            .await
        }
        _ => Ok(String::new()),
    }
}

/// Build the reflection-on-result prompt: prior review + experiment
/// context. The LLM writes a fresh `record_review` reflecting the
/// empirical evidence.
async fn build_reflection_on_result_prompt(
    memory: &Memory,
    prompts: &Prompts,
    session_id: &str,
    goal: &str,
    preferences: &str,
    hypothesis_id: i64,
    experiment_id: i64,
) -> Result<String> {
    use crate::experiment::ExperimentRepo;
    let hyp_text = fetch_hypothesis_text(memory, hypothesis_id).await?;
    let exp_repo = ExperimentRepo::new(memory.db_arc());
    let exp = exp_repo
        .get(experiment_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("reflection_on_result: experiment {experiment_id} not found"))?;
    // Most recent review of this hypothesis (scope=review) — the
    // pre-experiment peer review.
    let prior_review = fetch_latest_review(memory, hypothesis_id, session_id).await;
    // Most recent experiment_result semantic note for this session —
    // the LLM's verdict on the experiment.
    let result_query = memory
        .db()
        .conn()
        .query(
            "SELECT summary, details_json FROM semantic_memories
             WHERE run_id = ?1 AND scope = 'experiment_result'
             ORDER BY id DESC LIMIT 1",
            [session_id],
        )
        .await;
    let result_summary_row = match result_query {
        Ok(mut rows) => rows.next().await.ok().flatten(),
        Err(_) => None,
    };
    let (result_summary, result_details_json) = match result_summary_row {
        Some(r) => (
            r.get::<String>(0).unwrap_or_else(|_| "(no summary)".into()),
            r.get::<String>(1).unwrap_or_else(|_| "{}".into()),
        ),
        None => ("(no evaluate_result recorded)".into(), "{}".into()),
    };
    let mut ctx = PromptContext::new();
    ctx.set("goal", goal);
    ctx.set("preferences", preferences);
    ctx.set("hypothesis_id", &hypothesis_id.to_string());
    ctx.set("hypothesis_text", &hyp_text);
    ctx.set("experiment_id", &exp.id.to_string());
    ctx.set("metric_name", &exp.metric_name);
    ctx.set(
        "metric_value",
        &exp.metric_value.map(|v| v.to_string()).unwrap_or_else(|| "null".into()),
    );
    ctx.set("verdict", "(see result_summary)");
    ctx.set("result_summary", &result_summary);
    ctx.set("result_details", &result_details_json);
    ctx.set("prior_review", &prior_review);
    prompts
        .render(AgentMode::ReflectionOnResult, &ctx)
        .context("rendering reflection_on_result prompt")
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

/// Build a self-correction prompt to recover from a ranking turn
/// whose marker failed to dispatch. The retry message re-asserts the
/// marker format (with explicit warnings about the common
/// prefix-vs-op-name confusion) and re-includes the original user
/// prompt so the LLM still has the hypothesis IDs in context.
///
/// `attempt` is 1-indexed so the LLM can see this is a retry, not a
/// fresh task. Two retries max (see [`RANKING_MAX_RETRIES`]); after
/// that the orchestrator gives up and the supervisor's idle-injection
/// will re-enqueue the ranking task at the next idle tick.
fn build_ranking_retry_prompt(original_user_prompt: &str, attempt: u32) -> String {
    format!(
        "{original}\n\n---\n\n\
         ## Retry {attempt}/{max} — your previous marker was rejected\n\n\
         The marker you emitted in your last response was not accepted. \
         The most likely reasons, in order of observed frequency:\n\n\
         1. **Wrong op name.** The op name slot must be exactly \
         `record_tournament_match`. It must NOT be `memory_op`, \
         `MEMORY_OP`, or any other name. `MEMORY_OP` is the marker \
         prefix that already appears in the syntax — putting it in \
         the op name slot is the most common mistake.\n\
         2. **Wrong payload schema.** The payload must be a JSON \
         object with EXACTLY these four fields and no others:\n\
            - `hypothesis_a` (integer) — the first hypothesis ID from \
         the prompt above\n\
            - `hypothesis_b` (integer) — the second hypothesis ID from \
         the prompt above\n\
            - `winner` (integer) — `1` if hypothesis_a wins, `2` if \
         hypothesis_b wins, `0` for a draw. Must be an integer, not \
         a string.\n\
            - `rationale` (string) — your reasoning explaining the \
         decision.\n\
         3. **Wrapped the marker.** Do not put the marker inside code \
         fences, quotes, or any other delimiter. Just emit it as a \
         line on its own.\n\n\
         Please re-emit ONE marker, on its own line, in exactly this \
         shape (substituting the actual hypothesis IDs from the prompt \
         above):\n\n\
         ```\n\
         [[MEMORY_OP:record_tournament_match:{{\"hypothesis_a\":<ID1>,\"hypothesis_b\":<ID2>,\"winner\":<1 or 2>,\"rationale\":\"<your reasoning>\"}}]]\n\
         ```",
        original = original_user_prompt,
        attempt = attempt,
        max = RANKING_MAX_RETRIES,
    )
}
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

/// Build the right experiment-mode prompt based on `mode`. Each branch
/// pulls a small amount of context (hypothesis text, experiment row,
/// prior review) and renders the matching template. On render error
/// we fall back to a minimal prompt so the LLM still has something to
/// act on (and the task doesn't get dropped from the queue).
async fn build_experiment_prompt(
    memory: &Memory,
    prompts: &Prompts,
    goal: &str,
    preferences: &str,
    mode: &str,
    experiment_id: Option<i64>,
    hypothesis_id: Option<i64>,
) -> Result<String> {
    use crate::experiment::ExperimentRepo;
    let exp_repo = ExperimentRepo::new(memory.db_arc());
    let mut ctx = PromptContext::new();
    ctx.set("goal", goal);
    ctx.set("preferences", preferences);

    // Try to load the experiment row (for execute / evaluate modes).
    let exp = match experiment_id {
        Some(id) => exp_repo.get(id).await.ok().flatten(),
        None => None,
    };

    let render_result: Result<String> = match mode {
        "experiment_design" => {
            let hyp_id = hypothesis_id
                .ok_or_else(|| anyhow::anyhow!("experiment_design needs hypothesis_id"))?;
            let hyp_text = fetch_hypothesis_text(memory, hyp_id).await?;
            ctx.set("hypothesis_id", &hyp_id.to_string());
            ctx.set("hypothesis_text", &hyp_text);
            prompts
                .render(AgentMode::ExperimentDesign, &ctx)
                .context("rendering experiment_design prompt")
        }
        "experiment_execute" => {
            let exp = exp
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("experiment_execute needs a valid experiment_id"))?;
            ctx.set("experiment_id", &exp.id.to_string());
            ctx.set("metric_name", &exp.metric_name);
            ctx.set("code", &exp.code);
            ctx.set("description", exp.error.as_deref().unwrap_or(""));
            prompts
                .render(AgentMode::ExperimentExecute, &ctx)
                .context("rendering experiment_execute prompt")
        }
        "experiment_evaluate" => {
            let exp = exp.as_ref().ok_or_else(|| {
                anyhow::anyhow!("experiment_evaluate needs a valid experiment_id")
            })?;
            let hyp_text = fetch_hypothesis_text(memory, exp.hypothesis_id).await?;
            ctx.set("experiment_id", &exp.id.to_string());
            ctx.set("hypothesis_id", &exp.hypothesis_id.to_string());
            ctx.set("hypothesis_text", &hyp_text);
            ctx.set("status", exp.status.as_str());
            ctx.set("metric_name", &exp.metric_name);
            ctx.set(
                "metric_value",
                &exp.metric_value
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "null".into()),
            );
            ctx.set(
                "exit_code",
                &exp.exit_code.map(|v| v.to_string()).unwrap_or_else(|| "null".into()),
            );
            ctx.set(
                "duration_ms",
                &exp.duration_ms.map(|v| v.to_string()).unwrap_or_else(|| "null".into()),
            );
            // Truncate stdout/stderr to keep the prompt under token budget.
            let trunc = |s: &Option<String>| -> String {
                s.as_deref()
                    .map(|x| {
                        if x.len() > 2000 {
                            format!("{}…(truncated)", &x[..2000])
                        } else {
                            x.to_string()
                        }
                    })
                    .unwrap_or_else(|| "(empty)".into())
            };
            ctx.set("stdout", &trunc(&exp.stdout));
            ctx.set("stderr", &trunc(&exp.stderr));
            prompts
                .render(AgentMode::ExperimentEvaluate, &ctx)
                .context("rendering experiment_evaluate prompt")
        }
        // Fallback so the queue doesn't drop the task entirely.
        _ => {
            ctx.set("hypothesis_id", "0");
            ctx.set("hypothesis_text", "(no hypothesis context)");
            prompts
                .render(AgentMode::ExperimentDesign, &ctx)
                .context("rendering fallback experiment prompt")
        }
    };
    match render_result {
        Ok(s) => Ok(s),
        Err(e) => {
            tracing::warn!(error = %e, mode, "experiment prompt render failed; using fallback");
            Ok(format!(
                "Run experiment_design / experiment_execute / experiment_evaluate as the mode suggests. \
                 Goal: {goal}. experiment_id: {:?}, hypothesis_id: {:?}.",
                experiment_id, hypothesis_id
            ))
        }
    }
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

/// One edge in the research pipeline DAG. Specifying the edge as
/// data (instead of nested match arms) lets the topology be read at a
/// glance and edited in one place — see architecture review §C6.
///
/// `next_agent` + `next_mode` identify the downstream task. `priority`
/// overrides the default enqueue priority. `requires_hypothesis`
/// indicates the edge fans out per-hypothesis (one follow-up per
/// hypothesis id).
#[derive(Debug, Clone)]
pub struct FollowUpSpec {
    pub from_agent: &'static str,
    pub from_mode: Option<&'static str>,
    pub next_agent: &'static str,
    pub next_mode: &'static str,
    pub priority: i64,
    pub requires_hypothesis: bool,
}

/// The pipeline DAG. Currently five edges: generation → reflection
/// (per hypothesis), reflection → ranking, evolution → reflection
/// (per hypothesis), and the three-stage experiment chain
/// (design → execute → evaluate → reflection_on_result).
///
/// New stages add one row here. New agents add a row to the table that
/// owns them, not a new arm to `enqueue_follow_ups`.
pub static FOLLOW_UP_SPECS: &[FollowUpSpec] = &[
    FollowUpSpec {
        from_agent: "generation",
        from_mode: None,
        next_agent: "reflection",
        next_mode: "reflection_review",
        priority: 100,
        requires_hypothesis: true,
    },
    FollowUpSpec {
        from_agent: "evolution",
        from_mode: None,
        next_agent: "reflection",
        next_mode: "reflection_review",
        priority: 100,
        requires_hypothesis: true,
    },
    FollowUpSpec {
        from_agent: "reflection",
        from_mode: None,
        next_agent: "ranking",
        next_mode: "ranking_pairwise",
        priority: 100,
        requires_hypothesis: false,
    },
    FollowUpSpec {
        from_agent: "experiment",
        from_mode: Some("experiment_design"),
        next_agent: "experiment",
        next_mode: "experiment_execute",
        priority: 110,
        requires_hypothesis: false,
    },
    FollowUpSpec {
        from_agent: "experiment",
        from_mode: Some("experiment_execute"),
        next_agent: "experiment",
        next_mode: "experiment_evaluate",
        priority: 120,
        requires_hypothesis: false,
    },
    FollowUpSpec {
        from_agent: "experiment",
        from_mode: Some("experiment_evaluate"),
        next_agent: "reflection",
        next_mode: "reflection_on_result",
        priority: 130,
        requires_hypothesis: false,
    },
];

/// One idle-injection edge in the DAG. Fired by the supervisor when
/// the queue drains — the predicate decides whether the edge is
/// eligible given current session state. The payload builder produces
/// the `serde_json::Value` to enqueue.
///
/// This is the second half of the DAG that previously lived inline
/// in `Supervisor::decide_next_steps` (a 110-line if-tree). Folding
/// it into a data table makes "what runs when the queue is empty?"
/// answerable by reading 4 rows.
#[derive(Debug, Clone, Copy)]
pub struct IdleSpec {
    pub label: &'static str,
    pub next_agent: &'static str,
    pub next_mode: &'static str,
    pub priority: i64,
    /// Returns `Some(args)` if the edge should fire — `args` is the
    /// extra payload fields beyond `(agent, mode, prompt, goal, preferences)`.
    /// Returns `None` to skip.
    pub predicate: fn(&IdleState) -> Option<serde_json::Value>,
}

/// Snapshot the idle predicates evaluate against. Built once per
/// idle tick by the supervisor.
#[derive(Debug, Clone)]
pub struct IdleState {
    pub session_id: String,
    pub goal: String,
    pub preferences: String,
    pub total_hypotheses: i64,
    pub mature_hypotheses: usize,
    pub match_count: usize,
    pub last_meta_review_at: usize,
    pub meta_review_interval: usize,
    pub min_hypotheses: usize,
    pub min_mature: usize,
    pub top_hypotheses_empty: bool,
    pub hypothesis_needing_experiment: Option<i64>,
}

/// The idle-injection DAG. Rows in priority order (lower = earlier
/// in the supervisor's tick).
///
/// Predicates return the extra payload fields needed for the task.
/// `agent`, `mode`, `goal`, `preferences` are added by the supervisor
/// before enqueue.
pub static IDLE_SPECS: &[IdleSpec] = &[
    IdleSpec {
        label: "ranking/RunTournamentBatch",
        next_agent: "ranking",
        next_mode: "ranking_pairwise",
        priority: 120,
        predicate: |s| {
            if s.total_hypotheses >= s.min_hypotheses as i64 {
                Some(serde_json::json!({}))
            } else {
                None
            }
        },
    },
    IdleSpec {
        label: "evolution/EvolveTopHypotheses",
        next_agent: "evolution",
        next_mode: "evolution_combine",
        priority: 150,
        predicate: |s| {
            if s.mature_hypotheses >= s.min_mature && !s.top_hypotheses_empty {
                Some(serde_json::json!({}))
            } else {
                None
            }
        },
    },
    IdleSpec {
        label: "metareview/GenerateSystemFeedback",
        next_agent: "metareview",
        next_mode: "metareview_system",
        priority: 180,
        predicate: |s| {
            if s.match_count > 0
                && s.match_count - s.last_meta_review_at >= s.meta_review_interval
            {
                Some(serde_json::json!({}))
            } else {
                None
            }
        },
    },
    IdleSpec {
        label: "experiment/DesignForHypothesis",
        next_agent: "experiment",
        next_mode: "experiment_design",
        priority: 90, // below reflection/ranking so we don't crowd them out
        predicate: |s| {
            s.hypothesis_needing_experiment
                .map(|hyp_id| serde_json::json!({ "hypothesis_id": hyp_id }))
        },
    },
];

/// Find the matching follow-up spec for `(agent, mode)`. Returns `None`
/// for terminal agents (ranking, supervisor, metareview) whose
/// follow-ups are decided by the supervisor's idle-injection policy.
fn spec_for(agent_name: &str, mode: &str) -> Option<&'static FollowUpSpec> {
    FOLLOW_UP_SPECS.iter().find(|s| {
        s.from_agent == agent_name
            && s.from_mode.map(|m| m == mode).unwrap_or(true)
    })
}

/// Enqueue follow-up tasks based on which agent completed. Looks up
/// the matching `FollowUpSpec` in the [`FOLLOW_UP_SPECS`] DAG and
/// dispatches the downstream task. Per-hypothesis edges fan out one
/// task per id; per-session edges enqueue a single task with no
/// hypothesis reference.
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
    args: &Value,
) -> Result<()> {
    let mode = args.get("mode").and_then(|v| v.as_str()).unwrap_or("");
    let spec = match spec_for(agent_name, mode) {
        Some(s) => s,
        None => return Ok(()), // terminal agent
    };

    if spec.requires_hypothesis {
        // Per-hypothesis fan-out. Generation / evolution each produce
        // N hypotheses; we enqueue one reflection_review per id.
        if hypothesis_ids.is_empty() {
            return Ok(());
        }
        for &hyp_id in hypothesis_ids {
            let rendered = build_review_prompt(memory, prompts, goal, preferences, hyp_id)
                .await
                .unwrap_or_default();
            queue
                .enqueue(EnqueueRequest {
                    session_id: session_id.to_string(),
                    agent: spec.next_agent.to_string(),
                    action: "run_agent".to_string(),
                    payload: json!({
                        "agent": spec.next_agent,
                        "mode": spec.next_mode,
                        "prompt": rendered,
                        "hypothesis_id": hyp_id,
                        "goal": goal,
                        "preferences": preferences,
                    }),
                    priority: spec.priority,
                    max_attempts: 3,
                })
                .await?;
            info!(hyp_id, "enqueued {} → {}/{}", agent_name, spec.next_agent, spec.next_mode);
        }
        return Ok(());
    }

    // Single-task edge. Build the payload by combining fixed
    // fields (next_agent, next_mode) with whatever experiment ids
    // are available in `args`. Most edges don't need ids — the
    // downstream `build_prompt_for_agent` re-queries the DB.
    let mut payload = json!({
        "agent": spec.next_agent,
        "mode": spec.next_mode,
        "prompt": "",
        "goal": goal,
        "preferences": preferences,
    });
    if let Some(exp_id) = args.get("experiment_id").and_then(|v| v.as_i64()) {
        payload["experiment_id"] = json!(exp_id);
    }
    if let Some(hyp_id) = args.get("hypothesis_id").and_then(|v| v.as_i64()) {
        payload["hypothesis_id"] = json!(hyp_id);
    }

    // For the experiment_design edge: if the LLM didn't echo
    // experiment_id, recover it from the latest experiment for the
    // hypothesis. This preserves the original behaviour.
    if spec.from_mode == Some("experiment_design") && !payload.as_object().unwrap().contains_key("experiment_id") {
        let Some(hyp_id) = args.get("hypothesis_id").and_then(|v| v.as_i64()) else {
            return Ok(());
        };
        use crate::experiment::ExperimentRepo;
        let repo = ExperimentRepo::new(memory.db_arc());
        let Some(latest) = repo.latest_for_hypothesis(hyp_id).await? else {
            tracing::warn!("no experiment found for hypothesis {hyp_id}; chain broken");
            return Ok(());
        };
        payload["experiment_id"] = json!(latest.id);
    }

    queue
        .enqueue(EnqueueRequest {
            session_id: session_id.to_string(),
            agent: spec.next_agent.to_string(),
            action: "run_agent".to_string(),
            payload,
            priority: spec.priority,
            max_attempts: 3,
        })
        .await?;
    info!(
        "{} → {}/{} (priority {})",
        agent_name, spec.next_agent, spec.next_mode, spec.priority
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::registry::ToolRegistry;

    /// Same-key get_or_build returns the same Arc on repeated calls.
    /// This is the core invariant the per-session Runner cache exists
    /// to enforce — without it, every worker task would spawn a fresh
    /// Runner and pay the connect cost on every turn.
    #[tokio::test]
    async fn session_runners_cache_hit_returns_same_arc() {
        let cache = SessionRunners::new();
        let mem = crate::memory::Memory::new(db::open_memory().await.unwrap());
        let reg = Arc::new(ToolRegistry::new());
        let cfg = RunnerConfig::default();
        let a1 = cache.get_or_build("sess-1", "generation", mem.clone(), reg.clone(), cfg.clone());
        let a2 = cache.get_or_build("sess-1", "generation", mem.clone(), reg.clone(), cfg);
        assert!(Arc::ptr_eq(&a1, &a2), "cache miss returned a fresh Runner");
        assert_eq!(cache._len(), 1);
    }

    /// Different agents in the same session get distinct Runners
    /// (each owns its own Claude subprocess — they don't share state).
    #[tokio::test]
    async fn session_runners_distinct_agents_get_distinct_runners() {
        let cache = SessionRunners::new();
        let mem = crate::memory::Memory::new(db::open_memory().await.unwrap());
        let reg = Arc::new(ToolRegistry::new());
        let cfg = RunnerConfig::default();
        let gen_runner = cache.get_or_build("sess-1", "generation", mem.clone(), reg.clone(), cfg.clone());
        let rev = cache.get_or_build("sess-1", "reflection", mem.clone(), reg.clone(), cfg);
        assert!(!Arc::ptr_eq(&gen_runner, &rev));
        assert_eq!(cache._len(), 2);
    }

    /// Different sessions are isolated.
    #[tokio::test]
    async fn session_runners_distinct_sessions_get_distinct_runners() {
        let cache = SessionRunners::new();
        let mem = crate::memory::Memory::new(db::open_memory().await.unwrap());
        let reg = Arc::new(ToolRegistry::new());
        let cfg = RunnerConfig::default();
        let s1 = cache.get_or_build("sess-1", "generation", mem.clone(), reg.clone(), cfg.clone());
        let s2 = cache.get_or_build("sess-2", "generation", mem, reg, cfg);
        assert!(!Arc::ptr_eq(&s1, &s2));
        assert_eq!(cache._len(), 2);
    }

    /// Clearing the cache drops all entries (used by tests that need
    /// to start from a known-empty state).
    #[tokio::test]
    async fn session_runners_clear_drops_all_entries() {
        let cache = SessionRunners::new();
        let mem = crate::memory::Memory::new(db::open_memory().await.unwrap());
        let reg = Arc::new(ToolRegistry::new());
        let cfg = RunnerConfig::default();
        cache.get_or_build("sess-1", "generation", mem.clone(), reg.clone(), cfg.clone());
        cache.get_or_build("sess-1", "reflection", mem, reg, cfg);
        assert_eq!(cache._len(), 2);
        cache._clear();
        assert_eq!(cache._len(), 0);
    }

    /// The DAG must cover every existing follow-up edge. If you add a
    /// new edge, this test will not fail (you can add a row silently),
    /// but it catches accidental deletions.
    #[test]
    fn follow_up_specs_cover_all_known_edges() {
        // generation → reflection (per hypothesis)
        let g = spec_for("generation", "");
        assert!(g.is_some(), "generation must have a follow-up spec");
        let g = g.unwrap();
        assert_eq!(g.next_agent, "reflection");
        assert_eq!(g.next_mode, "reflection_review");
        assert!(g.requires_hypothesis);

        // evolution → reflection (per hypothesis)
        let e = spec_for("evolution", "");
        assert!(e.is_some());
        assert!(e.unwrap().requires_hypothesis);

        // reflection → ranking (one-shot)
        let r = spec_for("reflection", "");
        assert_eq!(r.unwrap().next_agent, "ranking");

        // 3-stage experiment chain
        assert_eq!(
            spec_for("experiment", "experiment_design").unwrap().next_mode,
            "experiment_execute"
        );
        assert_eq!(
            spec_for("experiment", "experiment_execute").unwrap().next_mode,
            "experiment_evaluate"
        );
        assert_eq!(
            spec_for("experiment", "experiment_evaluate").unwrap().next_mode,
            "reflection_on_result"
        );

        // Terminal agents have no spec
        assert!(spec_for("ranking", "").is_none());
        assert!(spec_for("supervisor", "parse_goal").is_none());
        assert!(spec_for("metareview", "metareview_system").is_none());
    }

    /// Edge case: an empty agent name must not collide with any
    /// legitimate (session, agent) key. Defensive — if a future
    /// caller ever passes "" as the agent, the cache must still
    /// return a unique Runner rather than overwriting a real one.
    #[tokio::test]
    async fn session_runners_empty_agent_name_is_distinct_slot() {
        let cache = SessionRunners::new();
        let mem = crate::memory::Memory::new(db::open_memory().await.unwrap());
        let reg = Arc::new(crate::registry::ToolRegistry::new());
        let cfg = RunnerConfig::default();
        let empty = cache.get_or_build("sess", "", mem.clone(), reg.clone(), cfg.clone());
        let gen_runner = cache.get_or_build("sess", "generation", mem, reg, cfg);
        assert!(!Arc::ptr_eq(&empty, &gen_runner));
        assert_eq!(cache._len(), 2);
    }

    /// Edge case: empty session id is distinct from any real
    /// session. Same defensive contract.
    #[tokio::test]
    async fn session_runners_empty_session_id_is_distinct_slot() {
        let cache = SessionRunners::new();
        let mem = crate::memory::Memory::new(db::open_memory().await.unwrap());
        let reg = Arc::new(crate::registry::ToolRegistry::new());
        let cfg = RunnerConfig::default();
        let empty = cache.get_or_build("", "generation", mem.clone(), reg.clone(), cfg.clone());
        let real = cache.get_or_build("real-sess", "generation", mem, reg, cfg);
        assert!(!Arc::ptr_eq(&empty, &real));
        assert_eq!(cache._len(), 2);
    }

    /// Concurrency: 32 simultaneous `get_or_build` calls on the
    /// same `(session, agent)` key must produce exactly ONE Runner.
    /// This pins the cache's atomic build semantics — without it, a
    /// race would create N Runners for the same key, defeating the
    /// whole point of the cache.
    #[tokio::test]
    async fn session_runners_concurrent_get_or_build_does_not_double_build() {
        let cache = Arc::new(SessionRunners::new());
        let mem = crate::memory::Memory::new(db::open_memory().await.unwrap());
        let reg = Arc::new(crate::registry::ToolRegistry::new());
        let cfg = RunnerConfig::default();

        let mut handles = Vec::new();
        for _ in 0..32 {
            let cache = Arc::clone(&cache);
            let mem = mem.clone();
            let reg = Arc::clone(&reg);
            let cfg = cfg.clone();
            handles.push(tokio::spawn(async move {
                cache.get_or_build("sess", "generation", mem, reg, cfg)
            }));
        }
        let mut arc = None;
        for h in handles {
            let got = h.await.unwrap();
            arc = Some(match arc {
                None => got,
                Some(prev) => {
                    assert!(
                        Arc::ptr_eq(&prev, &got),
                        "concurrent build produced two distinct Runners"
                    );
                    prev
                }
            });
        }
        assert_eq!(cache._len(), 1, "cache must contain exactly one slot");
    }

    /// Concurrency: 32 simultaneous `get_or_build` calls across
    /// DIFFERENT agents must produce 32 distinct Runners.
    /// Sanity-check for the race: the same code path that prevents
    /// double-build for one key must NOT collapse different keys.
    #[tokio::test]
    async fn session_runners_concurrent_distinct_keys_yield_distinct_runners() {
        let cache = Arc::new(SessionRunners::new());
        let mem = crate::memory::Memory::new(db::open_memory().await.unwrap());
        let reg = Arc::new(crate::registry::ToolRegistry::new());
        let cfg = RunnerConfig::default();
        let agents = [
            "generation",
            "reflection",
            "ranking",
            "evolution",
            "metareview",
            "experiment",
            "supervisor",
        ];

        let mut handles = Vec::new();
        for a in agents {
            let cache = Arc::clone(&cache);
            let mem = mem.clone();
            let reg = Arc::clone(&reg);
            let cfg = cfg.clone();
            let a = a.to_string();
            handles.push(tokio::spawn(async move {
                cache.get_or_build("sess", &a, mem, reg, cfg)
            }));
        }
        let mut arcs = Vec::new();
        for h in handles {
            arcs.push(h.await.unwrap());
        }
        // All distinct.
        for (i, a) in arcs.iter().enumerate() {
            for (j, b) in arcs.iter().enumerate() {
                if i != j {
                    assert!(!Arc::ptr_eq(a, b), "distinct agents must yield distinct Runners");
                }
            }
        }
        assert_eq!(cache._len(), agents.len());
    }

    /// `_clear` while another task holds an Arc to a Runner must
    /// not panic — the Arc keeps the Runner alive, and the next
    /// `get_or_build` simply builds a fresh one.
    #[tokio::test]
    async fn session_runners_clear_does_not_panic_with_live_arcs() {
        let cache = Arc::new(SessionRunners::new());
        let mem = crate::memory::Memory::new(db::open_memory().await.unwrap());
        let reg = Arc::new(crate::registry::ToolRegistry::new());
        let cfg = RunnerConfig::default();
        let live_arc = cache.get_or_build("sess", "generation", mem.clone(), reg.clone(), cfg.clone());
        cache._clear();
        // Arc still alive — no use-after-free.
        assert_eq!(Arc::strong_count(&live_arc), 1);
        // Next build produces a fresh Runner (the map slot is gone).
        let rebuilt = cache.get_or_build("sess", "generation", mem, reg, cfg);
        assert!(!Arc::ptr_eq(&live_arc, &rebuilt));
        assert_eq!(cache._len(), 1);
    }

    /// `spec_for` must return `None` for unknown (agent, mode)
    /// pairs — not panic, not return a wrong spec. Note that
    /// non-experiment agents have `from_mode: None` (they're
    /// mode-agnostic), so unknown modes on those agents
    /// legitimately match — only unknown *agents* return None.
    #[test]
    fn spec_for_unknown_returns_none() {
        // Unknown agent: no spec, regardless of mode.
        assert!(spec_for("nope", "").is_none());
        assert!(spec_for("nope", "anything").is_none());
        // experiment is mode-aware — an unknown mode on experiment
        // must NOT match (otherwise we'd silently route to the
        // wrong downstream agent).
        assert!(spec_for("experiment", "not_a_real_stage").is_none());
    }

    /// `spec_for` must be mode-aware for the experiment chain:
    /// `experiment_design`, `experiment_execute`, `experiment_evaluate`
    /// each pick up the right downstream spec. Unknown modes
    /// attached to `experiment` must NOT match any spec (otherwise
    /// we'd silently route the wrong follow-up).
    #[test]
    fn spec_for_experiment_modes_are_distinct() {
        let design = spec_for("experiment", "experiment_design");
        let execute = spec_for("experiment", "experiment_execute");
        let evaluate = spec_for("experiment", "experiment_evaluate");
        let bogus = spec_for("experiment", "experiment_foobar");
        assert!(design.is_some());
        assert!(execute.is_some());
        assert!(evaluate.is_some());
        assert!(bogus.is_none(), "unknown experiment mode must not match any spec");
        assert_ne!(design.unwrap().next_mode, execute.unwrap().next_mode);
        assert_ne!(execute.unwrap().next_mode, evaluate.unwrap().next_mode);
    }

    /// `FOLLOW_UP_SPECS` must be mode-aware for at least the
    /// experiment chain. If someone adds a generic `experiment → X`
    /// row with `from_mode: None`, the mode-aware rows above would
    /// still take precedence (they appear first), but a future
    /// reordering or rename could break this. Pin the precedence
    /// explicitly.
    #[test]
    fn follow_up_specs_mode_aware_rows_take_precedence() {
        // The mode-aware experiment rows must appear in the table.
        let modes: Vec<Option<&str>> = FOLLOW_UP_SPECS
            .iter()
            .filter(|s| s.from_agent == "experiment")
            .map(|s| s.from_mode)
            .collect();
        assert!(modes.contains(&Some("experiment_design")));
        assert!(modes.contains(&Some("experiment_execute")));
        assert!(modes.contains(&Some("experiment_evaluate")));
        // And no duplicate (from_agent, from_mode) pairs.
        let mut seen = std::collections::HashSet::new();
        for s in FOLLOW_UP_SPECS {
            let key = (s.from_agent, s.from_mode);
            assert!(
                seen.insert(key),
                "duplicate FOLLOW_UP_SPECS row: agent={:?} mode={:?}",
                s.from_agent,
                s.from_mode
            );
        }
    }
}
