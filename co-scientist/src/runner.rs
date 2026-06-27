//! The external orchestration loop.
//!
//! `Runner` is the "agent loop" that ante doesn't give you. It owns:
//!   - a [`Memory`] handle (the DB-backed memory API)
//!   - an optional [`Prompts`] registry with the 14 community prompt
//!     templates (used to build per-agent system prompts)
//!   - a `Claude` client (the LLM)
//!
//! For each user turn the runner:
//!   1. **Hook equivalent** — `log_event("turn_started", ...)` (turn-boundary,
//!      not step-boundary; this is the closest "hook" we can wire from
//!      outside without forking ante).
//!   2. **Prompt build** — composes the agent's role + the community
//!      prompt for the requested mode + the prior self-critique + a
//!      tool summary into a system prompt.
//!   3. **Plugin dispatch** — calls `claude.query(prompt)`, parses memory
//!      markers out of the response, and dispatches each one to the
//!      memory API. Tool-name aliases (`record_hypothesis` etc. ->
//!      `save_semantic` etc.) are applied automatically so the
//!      community's prompt wording works as-is.
//!   4. **Hook equivalent** — `log_event("turn_completed", ...)` with the
//!      cleaned response and the dispatched marker count.
//!   5. Returns the cleaned text to the caller.
//!
//! No ante code is modified. We treat `Claude` as an opaque LLM primitive.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use serde_json::json;
use tracing::{debug, info, warn};

use crate::agents::{Agent, AGENTS};
use crate::memory::{ContextLimits, Memory};
use crate::marker_normalizer;
use crate::prompts::Prompts;
use crate::registry::{default_allowlist, ToolRegistry};
use crate::bus::{EventBus, MemoryEvent};
use crate::skill::{parse_markers, Marker, SKILL};
use crate::tool::ToolCtx;
pub use crate::llm_query::BASE_BACKOFF_MS;
use crate::llm_query::{is_transient_error, jitter, persist_trace};

/// Closing reminder appended to every user prompt. The LLM frequently
/// forgets to emit a marker by the end of its response — putting this
/// right before the call keeps it fresh. Hoisted to a named constant
/// so the wording is testable and grep-able.
pub const CLOSING_REMINDER: &str = "\n\n---\n\
    IMPORTANT: When you have your answer, emit exactly ONE marker line:\n\
    [[MEMORY_OP:<tool_name>:{<required JSON fields>}]]\n\
    Do NOT just describe your answer in prose. The marker IS your tool call.\n\
    ---";

#[derive(Debug, Clone)]
pub struct RunnerConfig {
    pub model: String,
    pub max_turns: u32,
    /// Per-run event log cap for `get_context` recall.
    pub context_events: usize,
    pub context_semantic: usize,
    pub context_behavior: usize,
    /// Timeout for `claude.connect()` (spawning the CLI child). 0 = no
    /// timeout. Default: 30s.
    pub connect_timeout: Duration,
    /// Timeout for a single `claude.query()` (one turn). 0 = no
    /// timeout. Default: 5 min.
    pub query_timeout: Duration,
    /// How many semantic memories to fetch for `prior_session_summary`.
    /// Default: 10.
    pub prior_semantic_limit: usize,
    /// How many behavior memories to fetch for `prior_session_summary`.
    /// Default: 5.
    pub prior_behavior_limit: usize,
    /// Hard cap on rendered `prior_session_summary` length (chars).
    /// 0 = unlimited. Default: 4000.
    pub prior_max_chars: usize,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            model: std::env::var("CO_SCIENTIST_MODEL").unwrap_or_else(|_| "sonnet".to_string()),
            // Use higher max_turns so claude CLI enters tool-use mode
            // and properly invokes our markers. With max_turns=1 the
            // LLM responds conversationally without using tools.
            max_turns: 3,
            context_events: 20,
            context_semantic: 8,
            context_behavior: 5,
            connect_timeout: Duration::from_secs(30),
            query_timeout: Duration::from_secs(5 * 60),
            prior_semantic_limit: 10,
            prior_behavior_limit: 5,
            prior_max_chars: 4000,
        }
    }
}

pub struct Runner {
    pub memory: Memory,
    pub registry: Arc<ToolRegistry>,
    pub prompts: Option<Arc<Prompts>>,
    pub run_id: String,
    pub config: RunnerConfig,
    claude: Option<ClaudeHandle>,
    step_index: i64,
}

struct ClaudeHandle {
    client: crate::claude_cli::ClaudeCli,
}

impl Runner {
    /// Build a runner. `memory` is shared (cheap clone). `run_id` is the
    /// session id used in all `events`/`semantic_memories` rows.
    ///
    /// If `registry` is `None`, a new registry is built with the built-in
    /// memory tools (`save_semantic`, `save_behavior`, `get_context`).
    pub fn new(
        memory: Memory,
        run_id: impl Into<String>,
        config: RunnerConfig,
    ) -> Self {
        let mut reg = ToolRegistry::new();
        reg.register_all(crate::tool::builtin_tools());
        let prompts = Prompts::new().ok().map(Arc::new);
        if prompts.is_none() {
            tracing::warn!("community prompts failed to load; runner will use role-only system prompts");
        }
        Self {
            memory,
            registry: Arc::new(reg),
            prompts,
            run_id: run_id.into(),
            config,
            claude: None,
            step_index: 0,
        }
    }

    /// Build with a caller-supplied registry. Use this when you've
    /// loaded extra tools (e.g. from the skill loader) that you want
    /// the runner to be able to dispatch.
    pub fn with_registry(
        memory: Memory,
        registry: Arc<ToolRegistry>,
        run_id: impl Into<String>,
        config: RunnerConfig,
    ) -> Self {
        let prompts = Prompts::new().ok().map(Arc::new);
        Self {
            memory,
            registry,
            prompts,
            run_id: run_id.into(),
            config,
            claude: None,
            step_index: 0,
        }
    }

    /// Convenience: seed the 6 default agents from [`crate::agents::AGENTS`].
    /// Idempotent. The `system_prompt` column is populated with a
    /// per-agent summary of the modes it owns; the full prompts are
    /// loaded by [`crate::prompts::Prompts`].
    pub async fn seed_default_agents(&self) -> Result<()> {
        for a in AGENTS {
            let summary = format!(
                "Agent: {}\nRole: {}\nModes: {}",
                a.name,
                a.role,
                a.modes
                    .iter()
                    .map(|m| m.filename())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            self.memory
                .upsert_agent(a.name, a.role, &summary)
                .await
                .with_context(|| format!("seeding agent {}", a.name))?;
        }
        Ok(())
    }

    /// Build the system prompt for `agent` + `prior`. The prompt template
    /// is NOT rendered here — it belongs in the user message (which carries
    /// the actual task context like hypothesis text, reviews, etc.).
    ///
    /// The system prompt contains: role + tools docs + SKILL.md + prior notes.
    /// Agents that don't use memory tools (supervisor, ranking) get a
    /// trimmed prompt without SKILL.md or tool schemas.
    pub async fn build_system_prompt(
        &self,
        agent: &Agent,
        prior: &[crate::memory::BehaviorMemory],
        prior_session: &str,
    ) -> Result<String> {
        let prior_block = format_prior_block(prior);
        let prior_session_block = if prior_session.trim().is_empty()
            || prior_session.contains("(no prior sessions)")
        {
            String::new()
        } else {
            format!("\n\n{prior_session}")
        };
        let allowed = self.tools_for_agent(agent.name);
        let needs_tools = !matches!(agent.name, "supervisor" | "ranking");
        let tools_block = if !needs_tools || allowed.is_empty() {
            String::new()
        } else {
            let mut s = String::from("\n\n## How to call tools\n\n");
            s.push_str("You MUST call tools by emitting this exact marker format in your response:\n\n");
            s.push_str("```\n[[MEMORY_OP:<tool_name>:{<json args>}]]\n```\n\n");
            s.push_str("The marker must appear on its own line. The JSON args must match the tool's required fields EXACTLY.\n");
            s.push_str("Failure to emit a valid marker means your work is NOT saved.\n\n");

            // Per-tool compact schema with concrete examples
            s.push_str("### Tools you can call (REQUIRED fields shown)\n\n");
            for t in &allowed {
                let schema = t.input_schema();
                let required: Vec<&str> = schema
                    .get("required")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|s| s.as_str()).collect())
                    .unwrap_or_default();
                s.push_str(&format!("**{}** — {}\n", t.name(), t.description().lines().next().unwrap_or("")));
                if required.is_empty() {
                    s.push_str("  (no required fields)\n");
                } else {
                    let fields: Vec<String> = required
                        .iter()
                        .map(|r| {
                            let prop = schema.get("properties").and_then(|p| p.get(r));
                            let type_hint = prop
                                .and_then(|p| p.get("type"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("any");
                            let desc = prop
                                .and_then(|p| p.get("description"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            if desc.is_empty() {
                                format!("`{}`: {}", r, type_hint)
                            } else {
                                format!("`{}`: {} — {}", r, type_hint, desc.lines().next().unwrap_or(""))
                            }
                        })
                        .collect();
                    s.push_str(&format!("  Required: {}\n", fields.join(", ")));
                }
                s.push_str(&format!("  Example: `[[MEMORY_OP:{}:{{...}}]]`\n\n", t.name()));
            }

            // 3-layer workflow documentation
            s.push_str("### Memory recall workflow (token-efficient)\n");
            s.push_str("For targeted recall, use the 3-layer pattern instead of get_context:\n");
            s.push_str("1. `peek_context` — scan compact one-liners (id + summary), ~10x cheaper\n");
            s.push_str("2. `get_timeline` — get events around a relevant memory\n");
            s.push_str("3. `get_observation` — fetch full detail for a specific id\n\n");
            s
        };
        let skill_block = if needs_tools {
            format!("\n\n{}", SKILL)
        } else {
            String::new()
        };
        let role_block = format!(
            "You are the `{}` agent.\n\nRole: {}\n\nModes owned: {}\n",
            agent.name,
            agent.role,
            agent
                .modes
                .iter()
                .map(|m| m.filename())
                .collect::<Vec<_>>()
                .join(", ")
        );

        // Fetch recent marker errors for this agent+run and inject them
        // as a feedback block so the LLM can self-correct on retry.
        let marker_errors = self
            .memory
            .recent_marker_errors(&self.run_id, agent.name, 3)
            .await;
        let errors_block = if marker_errors.is_empty() {
            String::new()
        } else {
            let mut s = String::from("\n\n## Previous turn marker errors\n");
            s.push_str("Your recent tool calls had errors. Review and fix your marker format:\n");
            for (op, err) in marker_errors {
                s.push_str(&format!("- `{}`: {}\n", op, err));
            }
            s.push_str("\nMake sure your marker is on its own line and JSON args match the required fields.\n");
            s
        };

        Ok(format!(
            "{}{}{}{}{}{}",
            role_block,
            skill_block,
            prior_block,
            prior_session_block,
            tools_block,
            errors_block
        ))
    }

    /// Connect to the `claude` CLI. Lazily called by [`Runner::turn`].
    ///
    pub async fn connect(&mut self, agent: &Agent) -> Result<()> {
        use crate::claude_cli::{ClaudeCli, ClaudeOptions, PermissionMode};
        let prior = self
            .memory
            .recent_behavior(agent.name, 5)
            .await
            .context("loading prior behavior notes")?;
        let prior_session = self
            .memory
            .prior_session_summary(
                agent.name,
                "",
                self.config.prior_semantic_limit,
                self.config.prior_behavior_limit,
                self.config.prior_max_chars,
            )
            .await
            .context("loading prior session summary")?;
        let full_system = self
            .build_system_prompt(agent, &prior, &prior_session)
            .await
            .context("building system prompt")?;
        let opts = ClaudeOptions {
            system_prompt: Some(full_system),
            model: Some(self.config.model.clone()),
            max_turns: Some(self.config.max_turns),
            permission_mode: Some(PermissionMode::BypassPermissions),
            // Allow tools so Claude CLI enters agent mode. Our tools
            // are dispatched via text markers, but Claude CLI still
            // needs to know it's in an interactive agent session.
            allowed_tools: vec!["Bash".to_string(), "Read".to_string()],
            ..Default::default()
        };
        let client = if self.config.connect_timeout.is_zero() {
            ClaudeCli::connect(opts)
                .await
                .context("connecting to claude CLI (is `claude` in PATH?)")?
        } else {
            match tokio::time::timeout(
                self.config.connect_timeout,
                ClaudeCli::connect(opts),
            )
            .await
            {
                Ok(Ok(c)) => c,
                Ok(Err(e)) => {
                    return Err(e).context("connecting to claude CLI (is `claude` in PATH?)")
                }
                Err(_) => {
                    return Err(anyhow::anyhow!(
                        "claude connect timed out after {:?}",
                        self.config.connect_timeout
                    ))
                }
            }
        };
        self.claude = Some(ClaudeHandle { client });
        info!(
            agent = %agent.name,
            run_id = %self.run_id,
            prior_notes = prior.len(),
            "claude connected"
        );
        Ok(())
    }

    /// Run a single user turn. The closest analogue to "one agent loop
    /// iteration" we can express from outside ante.
    ///
    /// This is a thin orchestrator: it calls four free functions, one
    /// per concern, so each is independently testable and the seam
    /// between them is explicit.
    pub async fn turn(&mut self, agent: &Agent, user_text: &str) -> Result<TurnOutcome> {
        let step = self.step_index;
        self.step_index += 1;

        let prompt = prepare_user_prompt(
            &self.memory,
            &self.run_id,
            agent,
            user_text,
            step,
            &self.config,
        )
        .await?;

        // Hook: turn_started with the rendered prompt as trace.
        self.memory
            .log_event_with_trace(
                &self.run_id,
                agent.name,
                step,
                "turn_started",
                Some(json!({ "user_text": user_text, "prompt_len": prompt.len() })),
                Some(&prompt),
                None,
            )
            .await?;

        if self.claude.is_none() {
            self.connect(agent).await?;
        }
        let handle = self
            .claude
            .as_mut()
            .expect("claude is connected (checked above)");

        let raw_response = run_turn_query(
            handle,
            &prompt,
            &self.run_id,
            self.step_index,
            self.config.query_timeout,
        )
        .await?;
        let raw_text = raw_response.assistant_text;

        // Parse + dispatch markers. Each marker is dispatched independently;
        // a failure on one marker does not abort the rest of the turn.
        let parsed = parse_markers(&raw_text);
        let mut dispatched = 0u32;
        for marker in &parsed.markers {
            match dispatch_marker(
                &self.memory,
                self.memory.bus(),
                &self.registry,
                &self.run_id,
                agent,
                marker,
                self.step_index,
            )
            .await
            {
                Ok(()) => dispatched += 1,
                Err(e) => warn!(?e, op = marker.op.as_str(), "memory op failed"),
            }
        }
        debug!(
            markers = parsed.markers.len(),
            dispatched, "memory markers processed"
        );

        // Hook: turn_completed with raw response as trace.
        self.memory
            .log_event_with_trace(
                &self.run_id,
                agent.name,
                step,
                "turn_completed",
                Some(json!({
                    "raw_len": raw_text.len(),
                    "cleaned_len": parsed.cleaned_text.len(),
                    "markers": parsed.markers.len(),
                    "dispatched": dispatched,
                })),
                None,
                Some(&raw_text),
            )
            .await?;

        Ok(TurnOutcome {
            cleaned_text: parsed.cleaned_text,
            markers: Arc::new(parsed.markers),
        })
    }

    /// Tools the given agent is allowed to call, in the per-agent
    /// allowlist order. Returned as a `Vec<Arc<dyn Tool>>` so callers
    /// can render schemas or dispatch calls.
    pub fn tools_for_agent(&self, agent_name: &str) -> Vec<Arc<dyn crate::tool::Tool>> {
        let allow = match default_allowlist(agent_name) {
            Some(a) => a,
            None => return Vec::new(),
        };
        self.registry.for_agent(agent_name, Some(&allow))
    }
}

/// Build the user-message prompt for a turn.
///
/// On step 0, prepends a rendered `get_context` block (unless the
/// context layer reports nothing useful). Always appends the closing
/// reminder so the LLM is told the marker is its tool call right
/// before it's asked to emit one.
///
/// Free function (no `self`) so tests can pass any `Memory` and any
/// `RunnerConfig` without constructing a `Runner`.
pub async fn prepare_user_prompt(
    memory: &Memory,
    run_id: &str,
    agent: &Agent,
    user_text: &str,
    step_index: i64,
    config: &RunnerConfig,
) -> Result<String> {
    let mut prompt = user_text.to_string();
    if step_index == 0 {
        let ctx = memory
            .get_context(
                run_id,
                agent.name,
                user_text,
                ContextLimits {
                    events: config.context_events,
                    semantic: config.context_semantic,
                    behavior: config.context_behavior,
                    max_tokens: 0,
                    full_count: 3,
                },
            )
            .await?;
        if !ctx.rendered.trim_start().starts_with("(no prior context)") {
            prompt = format!("{}\n\n{}", ctx.rendered, user_text);
        }
    }
    prompt.push_str(CLOSING_REMINDER);
    Ok(prompt)
}

/// Dispatch a single parsed marker through the tool registry. The
/// marker's `op` is mapped to the tool name (with alias rewrite
/// for the community's tool names) and the payload becomes the
/// args. This is the single source of truth for what a marker can
/// do — adding a new tool is a registry entry, not a runner edit.
///
/// Free function: takes explicit deps so tests can construct one
/// without building a full `Runner`. The borrow checker already
/// forced `query_with_retry` to this shape; we follow precedent.
pub async fn dispatch_marker(
    memory: &Memory,
    bus: &EventBus,
    registry: &ToolRegistry,
    run_id: &str,
    agent: &Agent,
    marker: &Marker,
    step_index: i64,
) -> Result<()> {
    let raw_name = &marker.op;
    // Normalize: alias rewrite + scope/summary inference for
    // save_semantic. Pure function, unit-tested in
    // `marker_normalizer`. On rejection the Err propagates out of
    // dispatch_marker without ever touching the registry.
    let (tool_name, payload) = marker_normalizer::normalize(raw_name, marker.payload.clone())?;
    let ctx = ToolCtx {
        memory: memory.clone(),
        run_id: run_id.to_string(),
        agent_name: agent.name.to_string(),
    };
    match registry.dispatch(&tool_name, payload.clone(), &ctx).await {
        Ok(_out) => {
            memory
                .log_event(
                    run_id,
                    agent.name,
                    step_index,
                    "memory_op",
                    Some(json!({ "op": raw_name, "aliased_to": tool_name })),
                )
                .await?;
            Ok(())
        }
        Err(e) => {
            // Validation errors (missing required field, etc.) are the
            // model's fault, not the runtime's. Log at debug level —
            // the trace event below is the durable record. LLMs
            // frequently emit hallucinated tool names (e.g. "Write",
            // "add", "Edit" borrowed from native Claude Code tools).
            // These are noise, not errors to alert on.
            debug!(op = raw_name, error = %e, "tool dispatch failed");
            // Publish the meta-signal on the bus so a future reflection
            // pass can mine which tools fail most often per agent.
            // Recording key *names only* (not values) avoids PII leak
            // through the broadcast channel.
            let payload_keys: Option<Vec<String>> = payload
                .as_object()
                .map(|o| o.keys().cloned().collect());
            bus.publish(MemoryEvent::MarkerFailed {
                agent: agent.name.to_string(),
                op: raw_name.clone(),
                error: e.to_string(),
            });
            memory
                .log_event(
                    run_id,
                    agent.name,
                    step_index,
                    "memory_op_failed",
                    Some(json!({
                        "op": raw_name,
                        "aliased_to": tool_name,
                        "error": e.to_string(),
                        "payload_keys": payload_keys,
                    })),
                )
                .await
                .ok();
            Err(e)
        }
    }
}

#[derive(Debug, Clone)]
pub struct TurnOutcome {
    pub cleaned_text: String,
    pub markers: Arc<Vec<crate::skill::Marker>>,
}

/// Render the "## Your prior self-critique" block. The same function
/// is used by production (`Runner::build_system_prompt`) and by tests
/// (`tests/integration.rs`) so the contract can't drift.
pub fn format_prior_block(notes: &[crate::memory::BehaviorMemory]) -> String {
    if notes.is_empty() {
        return String::new();
    }
    let mut s = String::from("\n\n## Your prior self-critique\n");
    for b in notes {
        s.push_str(&format!("- {}: {}\n", b.pattern, b.notes));
    }
    s
}

/// Query the LLM with timeout + retry + exponential backoff.
///
/// The claude CLI subprocess is unreliable in several delightful ways:
/// network blips, stdout buffer fills, model returns mid-stream, process
/// crashes silently. A single 5-min timeout isn't enough — we need
/// to retry transient failures while preserving fast failure for
/// non-recoverable errors (auth, bad model, etc.).
///
/// Backoff: 0s, 1s, 3s between attempts (3 max). Errors classified by
/// `is_transient()` — auth/permission failures fail immediately.
///
/// Free function (no `self`) so tests can pass any `ClaudeHandle`
/// without constructing a `Runner`. Renamed from `query_with_retry` to
/// match the orchestrator naming (`prepare_user_prompt` / `run_turn_query` /
/// `dispatch_marker`).
async fn run_turn_query(
    handle: &mut ClaudeHandle,
    prompt: &str,
    run_id: &str,
    step_index: i64,
    query_timeout: Duration,
) -> Result<crate::claude_cli::TurnResponse> {
    const MAX_ATTEMPTS: u32 = 3;

    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..MAX_ATTEMPTS {
        let query_fut = handle.client.query(prompt.to_string());
        let query_result = if query_timeout.is_zero() {
            Ok(query_fut.await)
        } else {
            tokio::time::timeout(query_timeout, query_fut).await
        };

        match query_result {
            Ok(Ok(resp)) => {
                if attempt > 0 {
                    info!(attempt, "llm query succeeded after retry");
                }
                return Ok(resp);
            }
            Ok(Err(e)) => {
                let err_str = e.to_string();
                if is_transient_error(&err_str) && attempt + 1 < MAX_ATTEMPTS {
                    warn!(attempt, error = %err_str, "transient llm error, will retry");
                    last_err = Some(anyhow::Error::from(e));
                    let jitter_ms = jitter(step_index as u64, attempt as u64, 250);
                    let delay_ms = BASE_BACKOFF_MS[attempt as usize] + jitter_ms;
                    if let Err(write_err) = persist_trace(run_id, step_index, attempt, prompt) {
                        warn!(error = %write_err, "failed to persist trace");
                    }
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    continue;
                } else {
                    return Err(anyhow::Error::from(e).context("claude query failed"));
                }
            }
            Err(_) => {
                if attempt + 1 < MAX_ATTEMPTS {
                    warn!(
                        attempt,
                        timeout = ?query_timeout,
                        "llm query timed out, will retry"
                    );
                    let jitter_ms = jitter(step_index as u64, attempt as u64, 500);
                    let delay_ms = BASE_BACKOFF_MS[attempt as usize] + jitter_ms;
                    if let Err(write_err) = persist_trace(run_id, step_index, attempt, prompt) {
                        warn!(error = %write_err, "failed to persist trace");
                    }
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    continue;
                } else {
                    return Err(anyhow::anyhow!(
                        "claude query timed out after {:?} ({} attempts)",
                        query_timeout,
                        MAX_ATTEMPTS
                    ));
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("llm query failed after {} attempts", MAX_ATTEMPTS)))
}

/// Test-only helpers for the runner. Kept separate so the public API
/// stays narrow.
#[doc(hidden)]
pub mod test_helpers {
    // Re-export the production helper for tests so the
    // `test_helpers::format_prior_block` call site in
    // `tests/integration.rs` keeps working.
    pub use crate::runner::format_prior_block;
}
