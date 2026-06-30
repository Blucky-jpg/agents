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
use crate::skill_loader::{self, LoadedSkill};
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

/// Which turn of this Runner's lifetime we're on. Gates system-prompt
/// blocks that should only appear once: personality preamble, agent
/// skills one-liner. Replaces the old `shown_startup: bool` field that
/// conflated two unrelated concerns behind a single bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnPhase {
    /// Step 0 of this Runner — first call. Personality + skills preamble
    /// appear in the system prompt.
    FirstTurn,
    /// Any later call. Personality + skills preamble are dropped to save
    /// tokens; the model carries that awareness in its own context.
    Subsequent,
}

impl TurnPhase {
    pub fn from_runner(runner: &Runner) -> Self {
        if runner.turns_completed == 0 {
            TurnPhase::FirstTurn
        } else {
            TurnPhase::Subsequent
        }
    }
}

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
    /// Directory of `SKILL.md` files available as per-agent prompt
    /// content. Resolved via `set_skills_dir` on the runner. None
    /// (the default) means no agent-specific skill injection —
    /// equivalent to the pre-skills-field behaviour.
    pub skills_dir: Option<std::path::PathBuf>,
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
            skills_dir: None,
        }
    }
}

pub struct Runner {
    pub memory: Memory,
    pub registry: Arc<ToolRegistry>,
    pub prompts: Option<Arc<Prompts>>,
    pub run_id: String,
    pub config: RunnerConfig,
    /// Directory of `SKILL.md` files. The bodies are NOT rendered as
    /// prompt content — instead, the per-agent skill names are matched
    /// against registered tools and surfaced as a one-line preamble
    /// on the agent's first turn. After that, the model carries the
    /// awareness in its own context window; we stop re-injecting.
    skill_cache: std::sync::Mutex<Option<(std::path::PathBuf, Vec<LoadedSkill>)>>,
    /// Number of completed turns. Drives `TurnPhase` — see
    /// [`TurnPhase::from_runner`]. Replaces the old `shown_startup: bool`.
    turns_completed: u32,
    claude: Option<ClaudeHandle>,
    step_index: i64,
    /// In-process cache of the four DB-driven blocks that make up
    /// `build_system_prompt` (prior behavior, prior session summary,
    /// recent marker errors). Keyed by `(step_index, agent_name)`
    /// and invalidated on every `MemoryEvent::SemanticSaved` /
    /// `MemoryEvent::BehaviorSaved` published on the bus for this
    /// run. Eliminates 3–4 redundant DB hits per turn on long
    /// sessions. Replaces the previous "every turn refetches everything"
    /// path.
    ///
    /// Wrapped in `Arc` so the bus drain task can call `invalidate()`
    /// concurrently with the Runner's reads/writes.
    prompt_cache: Arc<PromptContextCache>,
    /// Join handle for the bus drain task. Kept so we can cancel it
    /// on Runner drop (future) — for now the task exits when the
    /// bus closes, which happens at process shutdown.
    _cache_drain: Option<tokio::task::JoinHandle<()>>,
}

struct ClaudeHandle {
    client: crate::claude_cli::ClaudeCli,
}

/// One cache slot for a single (step, agent) pair. Stores the four
/// DB-derived blocks needed to assemble the system prompt. Re-fetched
/// only when invalidated by a bus event or a new step.
#[derive(Debug, Clone)]
struct CachedPromptBlocks {
    prior_behavior: Vec<crate::memory::BehaviorMemory>,
    prior_session: String,
    marker_errors: Vec<(String, String)>,
    step_index: i64,
}

/// Prompt-context cache. Single-writer (the Runner), single-reader
/// (the Runner). The bus drain task invalidates via [`Arc::Self`].
#[derive(Debug)]
pub struct PromptContextCache {
    inner: std::sync::Mutex<Option<CachedPromptBlocks>>,
    /// Held only so the broadcast::Receiver isn't dropped — the
    /// drain task itself runs in a `tokio::spawn` and carries its own
    /// receiver clone.
    _subscriber_keepalive: Option<tokio::sync::broadcast::Receiver<crate::bus::MemoryEvent>>,
}

impl Default for PromptContextCache {
    fn default() -> Self {
        Self {
            inner: std::sync::Mutex::new(None),
            _subscriber_keepalive: None,
        }
    }
}

impl PromptContextCache {
    fn new() -> Self {
        Self::default()
    }

    /// Attach the bus subscription. Called from the Runner after the
    /// memory handle is available. Spawns a drain task that
    /// invalidates the cache on any `SemanticSaved` (matched on
    /// `run_id`) or `BehaviorSaved` (unconditional — bus doesn't
    /// carry run_id on that variant) event. Returns the join handle
    /// for tests that want to await the drain.
    ///
    /// This is what the doc-comment on `prompt_cache` (in the Runner
    /// struct) has always promised — but previously the field was
    /// `Option<()>` and never instantiated. Writes from
    /// `consolidation::cluster_and_archive`, `experiment_evaluate`,
    /// and other non-dispatch paths now correctly invalidate.
    fn attach_bus(
        self: &Arc<Self>,
        bus: crate::bus::EventBus,
        run_id: String,
    ) -> tokio::task::JoinHandle<()> {
        let mut rx = bus.subscribe();
        let cache = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(crate::bus::MemoryEvent::SemanticSaved {
                        run_id: ev_run, ..
                    }) => {
                        // Filter on run_id so a sibling session's
                        // saves don't invalidate our cache.
                        if ev_run == run_id {
                            cache.invalidate();
                        }
                    }
                    // BehaviorSaved doesn't carry run_id — but
                    // BehaviorSaved events are rare and the prior
                    // behavior block is global to the agent; be
                    // conservative and invalidate unconditionally.
                    Ok(crate::bus::MemoryEvent::BehaviorSaved { .. }) => {
                        cache.invalidate();
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // Lagged: we may have missed events. Be safe
                        // and invalidate — a redundant miss is much
                        // cheaper than a stale hit.
                        cache.invalidate();
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    _ => {}
                }
            }
        })
    }

    /// Get the cached blocks if they're fresh for `step_index`. A
    /// cache entry from a prior step is stale (the step counter moved
    /// on) and counts as a miss. Returns `None` on miss.
    fn get(&self, step_index: i64) -> Option<CachedPromptBlocks> {
        let guard = self.inner.lock().expect("prompt_cache poisoned");
        guard
            .as_ref()
            .filter(|c| c.step_index == step_index)
            .cloned()
    }

    /// Store freshly-fetched blocks for `step_index`.
    fn put(&self, blocks: CachedPromptBlocks) {
        let mut guard = self.inner.lock().expect("prompt_cache poisoned");
        *guard = Some(blocks);
    }

    /// Invalidate any cached entry. Called from the bus drain task
    /// when a new memory row is written that could change the
    /// retrieved set, and from the dispatch path after a successful
    /// marker dispatch (the latter is now redundant with the bus
    /// subscription, but kept as a fast-path to avoid one bus hop).
    fn invalidate(&self) {
        let mut guard = self.inner.lock().expect("prompt_cache poisoned");
        *guard = None;
    }

    /// How many cache slots are populated. Exposed for tests.
    #[doc(hidden)]
    pub fn _len(&self) -> usize {
        if self.inner.lock().expect("prompt_cache poisoned").is_some() {
            1
        } else {
            0
        }
    }
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
        let prompt_cache = Arc::new(PromptContextCache::new());
        let bus = memory.bus().clone();
        let run_id_for_drain = run_id.into();
        let cache_drain = prompt_cache.attach_bus(bus, run_id_for_drain.clone());
        let mut me = Self {
            memory,
            registry: Arc::new(reg),
            prompts,
            run_id: run_id_for_drain,
            config,
            skill_cache: std::sync::Mutex::new(None),
            turns_completed: 0,
            claude: None,
            step_index: 0,
            prompt_cache,
            _cache_drain: Some(cache_drain),
        };
        if let Some(dir) = &me.config.skills_dir {
            me.set_skills_dir(dir.clone());
        }
        me
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
        let prompts = match Prompts::new() {
            Ok(p) => Some(Arc::new(p)),
            Err(e) => {
                tracing::warn!(error = %e, "community prompts failed to load; runner will use role-only system prompts");
                None
            }
        };
        let prompt_cache = Arc::new(PromptContextCache::new());
        let bus = memory.bus().clone();
        let run_id_for_drain = run_id.into();
        let cache_drain = prompt_cache.attach_bus(bus, run_id_for_drain.clone());
        let mut me = Self {
            memory,
            registry,
            prompts,
            run_id: run_id_for_drain,
            config,
            skill_cache: std::sync::Mutex::new(None),
            turns_completed: 0,
            claude: None,
            step_index: 0,
            prompt_cache,
            _cache_drain: Some(cache_drain),
        };
        if let Some(dir) = &me.config.skills_dir {
            me.set_skills_dir(dir.clone());
        }
        me
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
    /// The system prompt contains: role + (first-turn only) personality +
    /// (first-turn only) agent skills preamble + tools docs + SKILL.md +
    /// prior notes. Agents that don't use memory tools (supervisor,
    /// ranking) get a trimmed prompt without SKILL.md or tool schemas.
    ///
    /// The four DB-derived blocks (prior behavior, prior session summary,
    /// marker errors, get_context) are fetched from [`PromptContextCache`]
    /// when possible, falling back to a direct DB hit on miss. The
    /// [`crate::bus::EventBus`] invalidates the cache automatically on
    /// every `SemanticSaved` / `BehaviorSaved` event.
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
        // Intersect the per-agent allowlist with `agent.skills`. Skills
        // named on the agent that are not in the registry are logged
        // and dropped from the visible tools block; registry tools not
        // named on the agent remain available (they are the built-in
        // memory ops like save_semantic / get_context).
        let visible_tools: Vec<_> = if agent.skills.is_empty() {
            allowed.iter().cloned().collect()
        } else {
            let skill_names: std::collections::HashSet<&str> =
                agent.skills.iter().copied().collect();
            let mut out = Vec::with_capacity(allowed.len());
            for t in &allowed {
                if skill_names.contains(t.name()) {
                    out.push(t.clone());
                }
            }
            let missing: Vec<&&str> = agent
                .skills
                .iter()
                .filter(|name| !allowed.iter().any(|t| t.name() == **name))
                .collect();
            for m in missing {
                tracing::warn!(agent = %agent.name, skill = %m, "skill not in registry — skipping");
            }
            out
        };
        let phase = TurnPhase::from_runner(self);
        let tools_block = if !needs_tools || visible_tools.is_empty() {
            String::new()
        } else {
            // 3-layer workflow documentation appears FIRST so a reader
            // finds the token-efficient recall pattern before the
            // schema-heavy tool reference. Previously this block sat
            // below the per-tool schemas; moving it up costs nothing
            // and saves a scroll.
            let mut s = String::from("\n\n### Memory recall workflow (token-efficient)\n");
            s.push_str("For targeted recall, use the 3-layer pattern instead of get_context:\n");
            s.push_str("1. `peek_context` — scan compact one-liners (id + summary), ~10x cheaper\n");
            s.push_str("2. `get_timeline` — get events around a relevant memory\n");
            s.push_str("3. `get_observation` — fetch full detail for a specific id\n\n");

            s.push_str("## How to call tools\n\n");
            s.push_str("You MUST call tools by emitting this exact marker format in your response:\n\n");
            s.push_str("```\n[[MEMORY_OP:<tool_name>:{<json args>}]]\n```\n\n");
            s.push_str("The marker must appear on its own line. The JSON args must match the tool's required fields EXACTLY.\n");
            s.push_str("Failure to emit a valid marker means your work is NOT saved.\n\n");
            // Hallucinated tool names are a recurring failure mode
            // (see dispatch_unknown_tool: LLMs invent `Write`, `add`,
            // `Edit`, `add_memory`, `note`, etc. borrowed from
            // native Claude Code or generic tool APIs). State the
            // exact allowlist up front so the model cannot substitute
            // a synonym. Listed as a comma-separated line rather than
            // a code block so it reads as a hard constraint, not an
            // example.
            s.push_str(&format!(
                "You may ONLY use these tool names (exact match, no synonyms, no prefixes): {}.\n",
                visible_tools.iter().map(|t| t.name()).collect::<Vec<_>>().join(", ")
            ));
            s.push_str("If you write any other name (e.g. `add_memory`, `note`, `Write`, `Edit`) the call is silently dropped — your work is lost.\n\n");

            // Per-tool compact schema with concrete examples
            s.push_str("### Tools you can call (REQUIRED fields shown)\n\n");
            for t in &visible_tools {
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
        let personality_block = match (phase, agent.personality.is_empty()) {
            (TurnPhase::FirstTurn, false) => format!("\nPersonality: {}\n", agent.personality),
            _ => String::new(),
        };
        let agent_skills_block = if phase == TurnPhase::FirstTurn {
            self.render_agent_skills(agent)
        } else {
            String::new()
        };

        // Fetch recent marker errors for this agent+run and inject them
        // as a feedback block so the LLM can self-correct on retry.
        // Use the cache if a fresh entry exists for this step — saves
        // one DB hit per turn on long sessions.
        let marker_errors = match self.prompt_cache.get(self.step_index) {
            Some(cached) => cached.marker_errors,
            None => self
                .memory
                .recent_marker_errors(&self.run_id, agent.name, 3)
                .await,
        };
        let errors_block = if marker_errors.is_empty() {
            String::new()
        } else {
            let mut s = String::from("\n\n## Previous turn marker errors\n");
            s.push_str("Your recent tool calls had errors. Review and fix your marker format:\n");
            for (op, err) in &marker_errors {
                s.push_str(&format!("- `{}`: {}\n", op, err));
            }
            s.push_str("\nMake sure your marker is on its own line and JSON args match the required fields.\n");
            s
        };

        Ok(format!(
            "{}{}{}{}{}{}{}{}",
            role_block,
            personality_block,
            agent_skills_block,
            skill_block,
            prior_block,
            prior_session_block,
            tools_block,
            errors_block
        ))
    }

    /// Connect to the `claude` CLI. Lazily called by [`Runner::turn`].
    ///
    /// Reads the four DB-derived blocks (prior behavior, prior session
    /// summary, marker errors) ONCE here — they live in the same
    /// [`PromptContextCache`] entry for the lifetime of this
    /// turn, so [`Runner::build_system_prompt`] gets them for free
    /// when the worker calls it.
    pub async fn connect(&mut self, agent: &Agent) -> Result<()> {
        use crate::claude_cli::{ClaudeCli, ClaudeOptions, PermissionMode};

        // Fetch (or hit cache) for the four DB-derived blocks needed
        // by build_system_prompt. The cache slot is keyed by the
        // current step_index; reuse across both calls within one
        // turn.
        let (prior, prior_session) = match self.prompt_cache.get(self.step_index) {
            Some(cached) => (cached.prior_behavior, cached.prior_session),
            None => {
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
                let marker_errors = self
                    .memory
                    .recent_marker_errors(&self.run_id, agent.name, 3)
                    .await;
                self.prompt_cache.put(CachedPromptBlocks {
                    prior_behavior: prior.clone(),
                    prior_session: prior_session.clone(),
                    marker_errors,
                    step_index: self.step_index,
                });
                (prior, prior_session)
            }
        };

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
    /// Thin wrapper: the entire turn path lives in
    /// [`Runner::run_turn_inner`]. This method always uses the
    /// blocking query path. Streaming callers use
    /// [`Runner::turn_stream`].
    pub async fn turn(&mut self, agent: &Agent, user_text: &str) -> Result<TurnOutcome> {
        self.run_turn_inner(agent, user_text, TurnStrategy::Blocking)
            .await
    }

    /// Stream a turn, pushing each newly-arrived chunk of raw
    /// (uncleaned) text into `delta_tx`. Marker parsing still happens once
    /// on the full response — markers live in the `result` frame, not in
    /// the intermediate `assistant` deltas, so we can't extract them
    /// incrementally.
    ///
    /// `delta_tx` is a `mpsc::UnboundedSender<String>` — `Send + 'static`
    /// so it can be moved across the `.await` calls inside the runner.
    /// Subscribers receive the corresponding events on the unbounded
    /// receiver and append each delta live.
    ///
    /// Behaviour matches `turn` for everything except text delivery: same
    /// prompt prep, same connect-on-demand, same retry policy, same
    /// marker dispatch, same trace hooks. Returns the same `TurnOutcome`
    /// once the turn completes.
    ///
    /// If `delta_tx` is `None`, falls back to the non-streaming `turn`
    /// behaviour — useful for callers that don't care about streaming.
    pub async fn turn_stream(
        &mut self,
        agent: &Agent,
        user_text: &str,
        delta_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    ) -> Result<TurnOutcome> {
        let strategy = match delta_tx {
            Some(tx) => TurnStrategy::Streaming(tx),
            None => TurnStrategy::Blocking,
        };
        self.run_turn_inner(agent, user_text, strategy).await
    }

    /// Shared turn implementation. The only difference between
    /// [`Runner::turn`] and [`Runner::turn_stream`] is which
    /// `ClaudeCli::query*` method the retry loop calls into. Pulling
    /// the rest of the orchestration into this method eliminated ~250
    /// LOC of near-duplicate code in the previous `turn` /
    /// `turn_stream` pair (see architecture review §C1).
    async fn run_turn_inner(
        &mut self,
        agent: &Agent,
        user_text: &str,
        strategy: TurnStrategy,
    ) -> Result<TurnOutcome> {
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

        let streamed = matches!(strategy, TurnStrategy::Streaming(_));
        let raw_response = match strategy {
            TurnStrategy::Blocking => {
                run_turn_query(
                    handle,
                    &prompt,
                    &self.run_id,
                    self.step_index,
                    self.config.query_timeout,
                )
                .await?
            }
            TurnStrategy::Streaming(tx) => {
                run_turn_query_stream(
                    handle,
                    &prompt,
                    &self.run_id,
                    self.step_index,
                    self.config.query_timeout,
                    tx,
                )
                .await?
            }
        };
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
                Ok(()) => {
                    // Successful tool calls write to semantic/behavior
                    // tables — these invalidate the prompt cache so
                    // the next turn sees the new state.
                    self.prompt_cache.invalidate();
                    dispatched += 1;
                }
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
                    "streamed": streamed,
                })),
                None,
                Some(&raw_text),
            )
            .await?;

        self.turns_completed += 1;
        Ok(TurnOutcome {
            cleaned_text: parsed.cleaned_text,
            markers: Arc::new(parsed.markers),
        })
    }

    /// The model this Runner is using. Resolved from
    /// `CO_SCIENTIST_MODEL` at construction time (via `RunnerConfig::default`).
    /// UI layers can use this to display the actual model name instead of
    /// re-reading the env var themselves.
    pub fn model(&self) -> &str {
        &self.config.model
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

    /// Point the runner at a directory of `SKILL.md` files. Skills
    /// discovered here are NOT registered as tools — they are
    /// rendered into the system prompt as additional instructions
    /// when the agent's `skills` field names them. Discover happens
    /// lazily on first prompt build; the cache is invalidated by a
    /// second call with a different path.
    pub fn set_skills_dir(&mut self, dir: impl Into<std::path::PathBuf>) {
        let path = dir.into();
        let mut cache = self.skill_cache.lock().expect("skill_cache poisoned");
        match cache.as_ref() {
            Some((cached_path, _)) if cached_path == &path => {}
            _ => *cache = Some((path, Vec::new())),
        }
    }

    /// Render the per-agent skill preamble. Shown only on the first
    /// turn (gated by `TurnPhase` at the call site). One line per
    /// allowed skill: `name — first line of description`. The full
    /// skill body is NOT inlined — the model invokes skills through
    /// the tool registry like any other CLI coding tool.
    fn render_agent_skills(&self, agent: &Agent) -> String {
        if agent.skills.is_empty() {
            return String::new();
        }
        let mut cache = self.skill_cache.lock().expect("skill_cache poisoned");
        let Some((path, skills)) = cache.as_mut() else {
            return String::new();
        };
        if skills.is_empty() {
            match skill_loader::discover(path) {
                Ok(loaded) => *skills = loaded,
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "skill discovery failed");
                    return String::new();
                }
            }
        }
        let by_name: std::collections::HashMap<&str, &LoadedSkill> =
            skills.iter().map(|s| (s.name.as_str(), s)).collect();
        let mut out = String::from("\n\n## Your callable skills\n");
        out.push_str("Call any of these by name when relevant. Each is a real tool in your registry.\n");
        let mut rendered_any = false;
        for name in agent.skills {
            match by_name.get(name) {
                Some(skill) => {
                    let one_liner = skill
                        .description
                        .lines()
                        .next()
                        .unwrap_or("")
                        .trim();
                    if one_liner.is_empty() {
                        out.push_str(&format!("- `{}`\n", skill.name));
                    } else {
                        out.push_str(&format!("- `{}` — {}\n", skill.name, one_liner));
                    }
                    rendered_any = true;
                }
                None => {
                    tracing::warn!(agent = %agent.name, skill = %name, "agent references unknown skill");
                    out.push_str(&format!("- `{}` (not found at {})\n", name, path.display()));
                }
            }
        }
        if rendered_any { out } else { String::new() }
    }

    /// Test-only: read the cache size (0 or 1 entry). Used by
    /// `runner.rs::tests` to verify cache hit/miss behaviour.
    #[doc(hidden)]
    pub fn _prompt_cache_len(&self) -> usize {
        self.prompt_cache._len()
    }

    /// Test-only: force a cache miss for the next call.
    #[doc(hidden)]
    pub fn _invalidate_prompt_cache(&self) {
        self.prompt_cache.invalidate();
    }

    /// Test-only: read the completed-turn counter. Replaces the old
    /// `shown_startup` field — tests assert on this directly.
    #[doc(hidden)]
    pub fn _turns_completed(&self) -> u32 {
        self.turns_completed
    }

    /// Test-only: bump the completed-turn counter without doing a
    /// real turn. Lets tests exercise `TurnPhase::Subsequent` paths
    /// without spawning a `claude` subprocess.
    #[doc(hidden)]
    pub fn _bump_turns_completed(&mut self) {
        self.turns_completed += 1;
    }
}

/// Strategy the turn path uses to deliver the LLM response. Replaces
/// the previous "copy-paste the entire turn body" duplication between
/// [`Runner::turn`] and [`Runner::turn_stream`].
enum TurnStrategy {
    Blocking,
    Streaming(tokio::sync::mpsc::UnboundedSender<String>),
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
///
/// On failure, writes a `memory_op_failed` event row in the `events`
/// table (durable trace, queryable by `recent_marker_errors` for the
/// self-correction loop). The accompanying `MemoryEvent::MarkerFailed`
/// bus publish is kept for future reflection passes — see
/// architecture review §C4. No subscriber exists today; the event is
/// a sink, but the schema is preserved so wiring a subscriber later
/// is a non-breaking change.
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
            let audit = memory
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
                .await;
            if let Err(audit_err) = audit {
                tracing::warn!(
                    op = %raw_name,
                    error = %audit_err,
                    "audit log of memory_op_failed failed; MarkerFailed event still published"
                );
            }
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

/// Same retry/timeout policy as `run_turn_query` but uses
/// `ClaudeCli::query_stream` so each incremental chunk of assistant
/// text is forwarded to `delta_tx` as soon as it arrives.
async fn run_turn_query_stream(
    handle: &mut ClaudeHandle,
    prompt: &str,
    run_id: &str,
    step_index: i64,
    query_timeout: Duration,
    delta_tx: tokio::sync::mpsc::UnboundedSender<String>,
) -> Result<crate::claude_cli::TurnResponse> {
    const MAX_ATTEMPTS: u32 = 3;

    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..MAX_ATTEMPTS {
        let tx = delta_tx.clone();
        let query_fut = handle.client.query_stream(prompt.to_string(), move |delta| {
            // Best-effort send: the receiver may have been dropped (e.g. UI
            // bailed). We don't propagate the error — `query_stream` returns
            // the full text at the end regardless.
            let _ = tx.send(delta.to_string());
        });
        let query_result = if query_timeout.is_zero() {
            Ok(query_fut.await)
        } else {
            tokio::time::timeout(query_timeout, query_fut).await
        };

        match query_result {
            Ok(Ok(resp)) => {
                if attempt > 0 {
                    info!(attempt, "llm stream query succeeded after retry");
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
                    return Err(anyhow::Error::from(e).context("claude stream query failed"));
                }
            }
            Err(_) => {
                if attempt + 1 < MAX_ATTEMPTS {
                    warn!(
                        attempt,
                        timeout = ?query_timeout,
                        "llm stream query timed out, will retry"
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
                        "claude stream query timed out after {:?} ({} attempts)",
                        query_timeout,
                        MAX_ATTEMPTS
                    ));
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("llm stream query failed after {} attempts", MAX_ATTEMPTS)))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::AGENTS;
    use crate::db;
    use crate::memory::Memory;

    async fn new_runner() -> Runner {
        // Tests share the on-disk DB path. The contract under test
        // is the prompt-build path, which never reads the DB, so a
        // shared Memory is fine. Async because `open_memory` needs a
        // tokio runtime.
        let mem = Memory::new(db::open_memory().await.unwrap());
        Runner::new(mem, "test-runner", RunnerConfig::default())
    }

    #[tokio::test]
    async fn personality_and_skills_preamble_appear_on_first_turn_only() {
        // generation has a non-empty personality, so we can assert
        // both the "shown" and "hidden" sides of the contract.
        let mut runner = new_runner().await;
        let agent = AGENTS.iter().find(|a| a.name == "generation").unwrap();

        let first = runner
            .build_system_prompt(agent, &[], "")
            .await
            .unwrap();
        assert!(
            first.contains("Cross-domain hunter"),
            "personality must be present on the first turn"
        );
        // generation.skills is empty by default, so no preamble —
        // but the underlying render path is the same. Verify both
        // gates: shown_startup=false => block path runs.
        assert!(
            first.contains("## Your callable skills") || agent.skills.is_empty(),
            "either preamble is shown or agent.skills is empty"
        );

        // Flip the phase the way `Runner::run_turn_inner` does after
        // the first response is parsed.
        runner._bump_turns_completed();

        let second = runner
            .build_system_prompt(agent, &[], "")
            .await
            .unwrap();
        assert!(
            !second.contains("Cross-domain hunter"),
            "personality must be dropped after the first turn"
        );
        assert!(
            !second.contains("## Your callable skills"),
            "skills preamble must be dropped after the first turn"
        );
    }

    #[test]
    fn turn_phase_distinguishes_first_and_subsequent() {
        let phase_first = TurnPhase::FirstTurn;
        let phase_next = TurnPhase::Subsequent;
        assert_ne!(phase_first, phase_next);
    }

    #[test]
    fn prompt_cache_hit_returns_same_blocks() {
        // Fresh cache starts empty.
        let cache = PromptContextCache::new();
        assert_eq!(cache._len(), 0);
        assert!(cache.get(0).is_none());

        // Insert at step 0.
        let blocks = CachedPromptBlocks {
            prior_behavior: Vec::new(),
            prior_session: "abc".to_string(),
            marker_errors: Vec::new(),
            step_index: 0,
        };
        cache.put(blocks.clone());
        assert_eq!(cache._len(), 1);

        // Step 0 hit returns the cached value.
        let cached = cache.get(0).expect("step 0 cache hit");
        assert_eq!(cached.prior_session, "abc");
        assert_eq!(cached.step_index, 0);

        // Step 1 is a miss — we want fresh data per step.
        assert!(cache.get(1).is_none());

        // Invalidate drops the slot.
        cache.invalidate();
        assert_eq!(cache._len(), 0);
        assert!(cache.get(0).is_none());
    }

    #[tokio::test]
    async fn render_agent_skills_emits_name_and_one_liner_per_skill() {
        // Build a synthetic agent with two fake skill names. We
        // populate the skill cache directly so the test does not
        // depend on filesystem discovery.
        let runner = new_runner().await;
        let mut cache = runner.skill_cache.lock().unwrap();
        *cache = Some((
            std::path::PathBuf::from("/nonexistent"),
            vec![
                LoadedSkill {
                    meta: crate::skill_loader::SkillFrontmatter {
                        name: Some("foo".into()),
                        description: Some("Does the foo thing well.".into()),
                        entrypoint: None,
                        timeout_seconds: 30,
                        inputs: None,
                    },
                    dir: std::path::PathBuf::from("/nonexistent/foo"),
                    body: "long body that must not be inlined".into(),
                    entrypoint: std::path::PathBuf::from("/nonexistent/foo/run.sh"),
                    name: "foo".into(),
                    description: "Does the foo thing well.".into(),
                },
                LoadedSkill {
                    meta: crate::skill_loader::SkillFrontmatter {
                        name: Some("bar".into()),
                        description: Some("Bar.\nSecond line that should be truncated.".into()),
                        entrypoint: None,
                        timeout_seconds: 30,
                        inputs: None,
                    },
                    dir: std::path::PathBuf::from("/nonexistent/bar"),
                    body: "".into(),
                    entrypoint: std::path::PathBuf::from("/nonexistent/bar/run.sh"),
                    name: "bar".into(),
                    description: "Bar.\nSecond line that should be truncated.".into(),
                },
            ],
        ));
        drop(cache);

        let agent = Agent {
            name: "test-agent",
            role: "test",
            modes: &[],
            personality: "",
            skills: &["foo", "bar", "missing"],
        };
        let out = runner.render_agent_skills(&agent);
        assert!(out.contains("- `foo` — Does the foo thing well."));
        // Only the first line of multi-line descriptions is used.
        assert!(out.contains("- `bar` — Bar."));
        assert!(!out.contains("Second line that should be truncated"));
        // Body content must not be inlined — the model calls skills
        // through the registry, not via prompt content.
        assert!(!out.contains("long body that must not be inlined"));
        // Missing skills get a warning line, not a silent drop.
        assert!(out.contains("`missing` (not found at"));
    }

    #[tokio::test]
    async fn render_agent_skills_returns_empty_when_agent_has_no_skills() {
        let runner = new_runner().await;
        let agent = AGENTS.iter().find(|a| a.name == "generation").unwrap();
        assert!(agent.skills.is_empty());
        assert_eq!(runner.render_agent_skills(agent), "");
    }

    // ── prepare_user_prompt edge cases ────────────────────────────
    //
    // The "loop should never break" invariant starts at the prompt
    // boundary. Each test pins a specific contract this function MUST
    // hold so the agent loop can recover from a malformed input
    // rather than panicking or producing a malformed prompt.

    /// Empty user_text must still produce a non-empty prompt: the
    /// closing reminder is mandatory. A turn with no body is unusual
    /// but legal (e.g. a follow-up "continue" turn).
    #[tokio::test]
    async fn prepare_user_prompt_empty_user_text_still_has_reminder() {
        let mem = Memory::new(db::open_memory().await.unwrap());
        let agent = &AGENTS[0]; // supervisor
        let cfg = RunnerConfig::default();
        let prompt = prepare_user_prompt(&mem, "run-x", &agent, "", 1, &cfg)
            .await
            .unwrap();
        assert!(prompt.contains(CLOSING_REMINDER));
        // step_index != 0 ⇒ no context prepended, so user_text is
        // literally empty plus the reminder.
        assert!(!prompt.contains("(no prior context)"));
    }

    /// Step 0 with no prior data must produce a prompt that says so
    /// explicitly (via the "(no prior context)" sentinel) — never
    /// silently omit the context section.
    #[tokio::test]
    async fn prepare_user_prompt_step_zero_no_prior_data() {
        let mem = Memory::new(db::open_memory().await.unwrap());
        let agent = &AGENTS[0];
        let cfg = RunnerConfig::default();
        let prompt = prepare_user_prompt(&mem, "fresh-run", &agent, "do thing", 0, &cfg)
            .await
            .unwrap();
        // Either we get an explicit "(no prior context)" marker, or
        // we get a context block — both are legal. The contract is
        // that the prompt does not silently miss the context section.
        let has_marker = prompt.contains("(no prior context)");
        let has_block = prompt.contains("do thing");
        assert!(has_marker || has_block, "prompt must surface the user_text");
        assert!(prompt.contains(CLOSING_REMINDER));
    }

    /// Closing reminder must appear exactly once per turn. If the
    /// implementation ever started appending it twice (e.g. via a
    /// double-call), token usage would silently double.
    #[tokio::test]
    async fn prepare_user_prompt_closing_reminder_appears_exactly_once() {
        let mem = Memory::new(db::open_memory().await.unwrap());
        let agent = &AGENTS[0];
        let cfg = RunnerConfig::default();
        for step in [0i64, 1, 5, 100] {
            let prompt = prepare_user_prompt(&mem, "run", &agent, "body", step, &cfg)
                .await
                .unwrap();
            let count = prompt.matches(CLOSING_REMINDER).count();
            assert_eq!(count, 1, "reminder duplicated at step {step}");
        }
    }

    /// User text with embedded newlines / unicode / braces must
    /// survive verbatim — the prompt builder does not normalize.
    #[tokio::test]
    async fn prepare_user_prompt_preserves_user_text_verbatim() {
        let mem = Memory::new(db::open_memory().await.unwrap());
        let agent = &AGENTS[0];
        let cfg = RunnerConfig::default();
        let body = "first line\nsecond line\n{\"json\":\"in user text\"}\n日本語 🚀";
        let prompt = prepare_user_prompt(&mem, "run", &agent, body, 1, &cfg)
            .await
            .unwrap();
        assert!(prompt.contains(body), "user text must round-trip");
    }

    /// `format_prior_block` is the test-helper that renders behavior
    /// notes into the system prompt. Edge cases: empty input,
    /// single note, notes with newlines and special characters.
    #[test]
    fn format_prior_block_empty_returns_empty_string() {
        let out = format_prior_block(&[]);
        assert_eq!(out, "");
    }

    #[test]
    fn format_prior_block_single_note_renders_one_bullet() {
        let notes = vec![crate::memory::BehaviorMemory {
            id: 1,
            agent_id: 1,
            pattern: "skip_empty".into(),
            notes: "do not generate empty sections".into(),
            evidence: None,
            created_at: "2026-01-01T00:00:00Z".into(),
        }];
        let out = format_prior_block(&notes);
        assert!(out.contains("## Your prior self-critique"));
        assert!(out.contains("- skip_empty: do not generate empty sections"));
    }

    #[test]
    fn format_prior_block_many_notes_render_in_order() {
        let mk = |pattern: &str, notes: &str| crate::memory::BehaviorMemory {
            id: 0,
            agent_id: 1,
            pattern: pattern.into(),
            notes: notes.into(),
            evidence: None,
            created_at: "2026-01-01T00:00:00Z".into(),
        };
        let notes = vec![
            mk("a", "first"),
            mk("b", "second"),
            mk("c", "third"),
        ];
        let out = format_prior_block(&notes);
        let pos_a = out.find("a: first").unwrap();
        let pos_b = out.find("b: second").unwrap();
        let pos_c = out.find("c: third").unwrap();
        assert!(pos_a < pos_b && pos_b < pos_c, "notes must render in input order");
    }

    /// `TurnPhase::from_runner` is the gate that decides whether to
    /// emit the personality + skills preamble. Edge cases: zero
    /// turns (must be FirstTurn), one completed turn (must be
    /// Subsequent), many completed turns (still Subsequent).
    #[tokio::test]
    async fn turn_phase_zero_turns_is_first() {
        let mut runner = new_runner().await;
        runner.turns_completed = 0;
        assert_eq!(TurnPhase::from_runner(&runner), TurnPhase::FirstTurn);
    }

    #[tokio::test]
    async fn turn_phase_one_completed_turn_is_subsequent() {
        let mut runner = new_runner().await;
        runner.turns_completed = 1;
        assert_eq!(TurnPhase::from_runner(&runner), TurnPhase::Subsequent);
    }

    #[tokio::test]
    async fn turn_phase_many_completed_turns_stays_subsequent() {
        let mut runner = new_runner().await;
        runner.turns_completed = 9999;
        assert_eq!(TurnPhase::from_runner(&runner), TurnPhase::Subsequent);
    }

    // ── PromptContextCache edge cases ──────────────────────────────
    //
    // The cache is the load-bearing performance invariant for long
    // sessions. Edge cases:
    //   - put after put: second one wins
    //   - get with stale step is a miss
    //   - invalidate then get is a miss
    //   - invalidate twice is idempotent

    #[test]
    fn prompt_cache_second_put_overwrites_first() {
        let cache = PromptContextCache::new();
        cache.put(CachedPromptBlocks {
            prior_behavior: Vec::new(),
            prior_session: "first".into(),
            marker_errors: Vec::new(),
            step_index: 0,
        });
        cache.put(CachedPromptBlocks {
            prior_behavior: Vec::new(),
            prior_session: "second".into(),
            marker_errors: Vec::new(),
            step_index: 0,
        });
        let cached = cache.get(0).unwrap();
        assert_eq!(cached.prior_session, "second");
    }

    #[test]
    fn prompt_cache_invalidate_is_idempotent() {
        let cache = PromptContextCache::new();
        cache.invalidate();
        cache.invalidate();
        cache.invalidate();
        assert_eq!(cache._len(), 0);
        assert!(cache.get(0).is_none());
    }

    /// Negative step_index should never crash. Defense against a
    /// future bug where a caller bumps the step counter wrong.
    #[test]
    fn prompt_cache_negative_step_does_not_crash() {
        let cache = PromptContextCache::new();
        cache.put(CachedPromptBlocks {
            prior_behavior: Vec::new(),
            prior_session: "x".into(),
            marker_errors: Vec::new(),
            step_index: -1,
        });
        let cached = cache.get(-1).expect("negative step should hit cache");
        assert_eq!(cached.prior_session, "x");
        assert!(cache.get(0).is_none(), "step 0 is a different slot");
        assert!(cache.get(-2).is_none(), "step -2 is a different slot");
    }

    /// Very large step_index — long sessions could reach thousands.
    /// The cache key is i64; i64::MAX is a valid step.
    #[test]
    fn prompt_cache_large_step_round_trips() {
        let cache = PromptContextCache::new();
        cache.put(CachedPromptBlocks {
            prior_behavior: Vec::new(),
            prior_session: "x".into(),
            marker_errors: Vec::new(),
            step_index: i64::MAX,
        });
        let cached = cache.get(i64::MAX).unwrap();
        assert_eq!(cached.step_index, i64::MAX);
    }

    /// Marker errors: cache hit returns the same Vec, including
    /// ordering.
    #[test]
    fn prompt_cache_preserves_marker_error_ordering() {
        let cache = PromptContextCache::new();
        let errors = vec![
            ("save_semantic".to_string(), "missing summary".to_string()),
            ("record_hypothesis".to_string(), "bad json".to_string()),
            ("peek_context".to_string(), "unknown id".to_string()),
        ];
        cache.put(CachedPromptBlocks {
            prior_behavior: Vec::new(),
            prior_session: String::new(),
            marker_errors: errors.clone(),
            step_index: 0,
        });
        let cached = cache.get(0).unwrap();
        assert_eq!(cached.marker_errors, errors);
    }

    // ── Runner::build_system_prompt edge cases ────────────────────
    //
    // The build_system_prompt function must never panic regardless
    // of agent state. These tests pin that contract.

    /// Agents with no modes (synthetic) get a modes line with no
    /// entries — should not panic, should still include role.
    #[tokio::test]
    async fn build_system_prompt_handles_agent_with_no_modes() {
        let runner = new_runner().await;
        let agent = Agent {
            name: "empty-agent",
            role: "an agent with no modes",
            modes: &[],
            personality: "minimal personality",
            skills: &[],
        };
        let prompt = runner
            .build_system_prompt(&agent, &[], "")
            .await
            .unwrap();
        assert!(prompt.contains("You are the `empty-agent` agent"));
        assert!(prompt.contains("Modes owned: "));
        assert!(prompt.contains("minimal personality"));
    }

    /// Supervisor and ranking agents get a tools block stripped
    /// (no memory tools). The system prompt must still produce valid
    /// output without the schema block.
    #[tokio::test]
    async fn build_system_prompt_supervisor_has_no_tools_block() {
        let runner = new_runner().await;
        let agent = AGENTS.iter().find(|a| a.name == "supervisor").unwrap();
        let prompt = runner
            .build_system_prompt(agent, &[], "")
            .await
            .unwrap();
        // The marker-format tutorial block is gated by `needs_tools`.
        // Supervisor needs_tools == false → no "How to call tools" block.
        assert!(
            !prompt.contains("How to call tools"),
            "supervisor should not see the tools block"
        );
        // The SKILL.md block is also gated.
        assert!(
            !prompt.contains("## Long-term memory"),
            "supervisor should not see the SKILL.md block"
        );
    }

    #[tokio::test]
    async fn build_system_prompt_ranking_has_no_tools_block() {
        let runner = new_runner().await;
        let agent = AGENTS.iter().find(|a| a.name == "ranking").unwrap();
        let prompt = runner
            .build_system_prompt(agent, &[], "")
            .await
            .unwrap();
        assert!(!prompt.contains("How to call tools"));
        assert!(!prompt.contains("## Long-term memory"));
    }

    /// Agent with non-empty personality AND a turn count > 0 must
    /// drop the personality block. This pins the "personality is a
    /// first-turn-only block" contract.
    #[tokio::test]
    async fn build_system_prompt_drops_personality_on_subsequent_turn() {
        let mut runner = new_runner().await;
        let agent = AGENTS.iter().find(|a| a.name == "generation").unwrap();
        runner._bump_turns_completed();
        let prompt = runner
            .build_system_prompt(agent, &[], "")
            .await
            .unwrap();
        assert!(!prompt.contains("Cross-domain hunter"));
        // But the role block is always present.
        assert!(prompt.contains("You are the `generation` agent"));
    }

    /// Tools block lists exactly the agent's allowlisted tools.
    /// This is the prompt↔registry contract: if the prompt says
    /// "tool X" but the agent can't dispatch X, the LLM will get
    /// confused. Allowlist is enforced at startup by
    /// prompt_allowlist::PromptToolTable::validate, but build_system_prompt
    /// must still emit the correct list.
    #[tokio::test]
    async fn build_system_prompt_lists_only_allowlisted_tools() {
        let runner = new_runner().await;
        // ranking is the most restrictive — only record_tournament_match.
        // ranking has needs_tools == false, so no tools block at all.
        let ranking = AGENTS.iter().find(|a| a.name == "ranking").unwrap();
        let ranking_prompt = runner
            .build_system_prompt(ranking, &[], "")
            .await
            .unwrap();
        assert!(!ranking_prompt.contains("**record_tournament_match**"));

        // generation has the research allowlist. It DOES get a tools block.
        let gen_agent = AGENTS.iter().find(|a| a.name == "generation").unwrap();
        let gen_prompt = runner
            .build_system_prompt(gen_agent, &[], "")
            .await
            .unwrap();
        assert!(gen_prompt.contains("**save_semantic**"));
        assert!(gen_prompt.contains("**record_hypothesis**"));
        // archive_observation is curator-only (supervisor / metareview).
        // Generation's allowlist does not include it, so it must NOT
        // appear in the rendered tools block.
        assert!(
            !gen_prompt.contains("**archive_observation**"),
            "curator-only tool leaked into generation's allowlist"
        );
        assert!(
            !gen_prompt.contains("**delete_observation**"),
            "curator-only tool leaked into generation's allowlist"
        );
    }

    /// The exact-allowlist warning block ("You may ONLY use these
    /// tool names") must appear for agents that DO get a tools
    /// block — this is the line that defends against hallucinated
    /// tool names. Pins §3 of architecture review (LLMs invent
    /// `Write`, `Edit`, `add`, etc.).
    #[tokio::test]
    async fn build_system_prompt_warns_against_hallucinated_tool_names() {
        let runner = new_runner().await;
        let gen_agent = AGENTS.iter().find(|a| a.name == "generation").unwrap();
        let prompt = runner
            .build_system_prompt(gen_agent, &[], "")
            .await
            .unwrap();
        assert!(prompt.contains("exact match, no synonyms, no prefixes"));
        assert!(prompt.contains("add_memory"));
        assert!(prompt.contains("Write"));
    }

    /// The 3-layer recall workflow block must be the FIRST sub-block
    /// under the tools section (was previously buried below the
    /// per-tool schemas). Pinning this prevents future refactors
    /// from putting it back at the bottom.
    #[tokio::test]
    async fn build_system_prompt_recall_workflow_above_tool_schemas() {
        let runner = new_runner().await;
        let gen_agent = AGENTS.iter().find(|a| a.name == "generation").unwrap();
        let prompt = runner
            .build_system_prompt(gen_agent, &[], "")
            .await
            .unwrap();
        let workflow_pos = prompt
            .find("Memory recall workflow (token-efficient)")
            .expect("3-layer workflow block must be present");
        let schemas_pos = prompt
            .find("Tools you can call")
            .expect("per-tool schema block must be present");
        assert!(
            workflow_pos < schemas_pos,
            "3-layer workflow must come BEFORE the per-tool schemas; \
             workflow at {workflow_pos}, schemas at {schemas_pos}"
        );
    }

    /// Empty prior_session string must NOT introduce a stray "(no
    /// prior sessions)" block — the gate is on the prior_session
    /// arg, not on whether the DB had rows.
    #[tokio::test]
    async fn build_system_prompt_empty_prior_session_no_block() {
        let runner = new_runner().await;
        let gen_agent = AGENTS.iter().find(|a| a.name == "generation").unwrap();
        let prompt = runner
            .build_system_prompt(gen_agent, &[], "")
            .await
            .unwrap();
        assert!(!prompt.contains("(no prior sessions)"));
    }

    /// The "(no prior sessions)" sentinel in prior_session must NOT
    /// be re-emitted — it's a marker for the producer (the DB
    /// layer), not for the LLM.
    #[tokio::test]
    async fn build_system_prompt_swallows_no_prior_sessions_sentinel() {
        let runner = new_runner().await;
        let gen_agent = AGENTS.iter().find(|a| a.name == "generation").unwrap();
        let prompt = runner
            .build_system_prompt(gen_agent, &[], "## From prior sessions\n(no prior sessions)\n")
            .await
            .unwrap();
        assert!(!prompt.contains("(no prior sessions)"));
    }

    // ── dispatch_marker edge cases ────────────────────────────────
    //
    // dispatch_marker is the seam between marker parsing and tool
    // dispatch. It MUST handle every combination of valid/invalid
    // marker without aborting the turn. These tests pin that.

    /// Community alias rewrite: `record_research_plan` must
    /// canonicalize to `save_semantic` (the runner should accept the
    /// community name). We test via the registry directly because
    /// dispatch_marker requires a live Memory + registry. The end-
    /// to-end path is covered by `save_semantic_via_registry_inserts_row`.
    #[tokio::test]
    async fn dispatch_marker_alias_rewrites_record_research_plan() {
        use crate::registry::ToolRegistry;
        use crate::tool::{builtin_tools, SaveSemanticTool, ToolCtx};
        let mem = Memory::new(db::open_memory().await.unwrap());
        let ctx = ToolCtx {
            memory: mem.clone(),
            run_id: "r".into(),
            agent_name: "supervisor".into(),
        };
        let mut reg = ToolRegistry::new();
        reg.register_all(builtin_tools());
        // Community name + a payload missing `scope`. The
        // marker_normalizer fills in `scope="plan"` from the alias
        // table.
        let marker = crate::skill::Marker {
            op: "record_research_plan".into(),
            payload: serde_json::json!({"summary": "plan summary"}),
        };
        // Should succeed — alias rewrite + scope fill-in.
        let out = dispatch_marker(&mem, mem.bus(), &reg, "r",
            AGENTS.iter().find(|a| a.name == "supervisor").unwrap(),
            &marker, 0)
            .await;
        assert!(out.is_ok(), "alias rewrite failed: {out:?}");
        // And the dispatched tool wrote a row.
        let _ = SaveSemanticTool;
    }

    /// Marker with an unknown op name must return Err (not panic)
    /// AND publish a MarkerFailed bus event AND write a
    /// memory_op_failed event row.
    #[tokio::test]
    async fn dispatch_marker_unknown_op_returns_err() {
        use crate::registry::ToolRegistry;
        use crate::tool::{builtin_tools, ToolCtx};
        let mem = Memory::new(db::open_memory().await.unwrap());
        let ctx = ToolCtx {
            memory: mem.clone(),
            run_id: "r".into(),
            agent_name: "generation".into(),
        };
        let mut bus_rx = mem.subscribe();
        let mut reg = ToolRegistry::new();
        reg.register_all(builtin_tools());
        let marker = crate::skill::Marker {
            op: "Add".into(), // "Add" is a hallucinated name
            payload: serde_json::json!({"x": 1}),
        };
        let result = dispatch_marker(
            &mem, mem.bus(), &reg, "r",
            AGENTS.iter().find(|a| a.name == "generation").unwrap(),
            &marker, 0,
        )
        .await;
        assert!(result.is_err(), "unknown op must return Err");

        // Drain the bus until we find the MarkerFailed event. There
        // may be other events in flight first (EventLogged for the
        // session row); skip them.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut saw_marker_failed = false;
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_millis(100), bus_rx.recv()).await {
                Ok(Ok(crate::bus::MemoryEvent::MarkerFailed { op, .. })) => {
                    assert_eq!(op, "Add");
                    saw_marker_failed = true;
                    break;
                }
                Ok(Ok(_)) => continue,
                _ => break,
            }
        }
        assert!(saw_marker_failed, "MarkerFailed must be published on the bus");
        let _ = ctx; // silence unused
    }

    /// Missing required field on a known tool must return Err AND
    /// the bus event must carry the canonical (rewritten) name, not
    /// the alias.
    ///
    /// We use `peek_context` (which requires `query`) because the
    /// `marker_normalizer` only validates `save_semantic` directly;
    /// other tools' required-field checks happen at the registry's
    /// `validate_args` step, which is the path that publishes
    /// `MarkerFailed`. Using save_semantic here would short-circuit
    /// at the normalizer before the bus publish.
    #[tokio::test]
    async fn dispatch_marker_missing_required_field_returns_err() {
        use crate::registry::ToolRegistry;
        use crate::tool::{builtin_tools, ToolCtx};
        let mem = Memory::new(db::open_memory().await.unwrap());
        let mut bus_rx = mem.subscribe();
        let mut reg = ToolRegistry::new();
        reg.register_all(builtin_tools());
        let marker = crate::skill::Marker {
            op: "peek_context".into(),
            payload: serde_json::json!({}), // missing required `query`
        };
        let result = dispatch_marker(
            &mem, mem.bus(), &reg, "r",
            AGENTS.iter().find(|a| a.name == "generation").unwrap(),
            &marker, 0,
        )
        .await;
        assert!(result.is_err(), "missing required field must return Err");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("missing required") || err_msg.contains("query"),
            "error must mention the missing field; got: {err_msg}"
        );
        // Bus event published. Drain until we find it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut saw = false;
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_millis(100), bus_rx.recv()).await {
                Ok(Ok(crate::bus::MemoryEvent::MarkerFailed { op, .. })) => {
                    assert_eq!(op, "peek_context");
                    saw = true;
                    break;
                }
                Ok(Ok(_)) => continue,
                _ => break,
            }
        }
        assert!(saw, "MarkerFailed must be published on the bus");
        let _ = ToolCtx {
            memory: mem,
            run_id: "r".into(),
            agent_name: "generation".into(),
        };
    }

    /// A successful dispatch must write a `memory_op` event row
    /// with both the raw op name and the canonical (rewritten) name
    /// in the payload — so post-hoc analysis can see what the LLM
    /// emitted vs. what was dispatched.
    #[tokio::test]
    async fn dispatch_marker_success_writes_event_with_alias() {
        use crate::registry::ToolRegistry;
        use crate::tool::{builtin_tools, ToolCtx};
        let mem = Memory::new(db::open_memory().await.unwrap());
        let mut reg = ToolRegistry::new();
        reg.register_all(builtin_tools());
        let marker = crate::skill::Marker {
            op: "record_research_plan".into(),
            payload: serde_json::json!({"summary": "plan summary"}),
        };
        dispatch_marker(
            &mem, mem.bus(), &reg, "r",
            AGENTS.iter().find(|a| a.name == "supervisor").unwrap(),
            &marker, 0,
        )
        .await
        .expect("dispatch ok");

        // Verify by querying the events table directly.
        let count = mem
            .conn()
            .query(
                "SELECT COUNT(*) FROM events WHERE run_id = ?1 AND type = 'memory_op'",
                ["r"],
            )
            .await
            .unwrap()
            .next()
            .await
            .unwrap()
            .map(|r| r.get::<i64>(0).unwrap())
            .unwrap_or(0);
        assert_eq!(count, 1, "exactly one memory_op event row");
        let _ = ToolCtx {
            memory: mem,
            run_id: "r".into(),
            agent_name: "supervisor".into(),
        };
    }

    /// End-to-end: a `run_python` marker dispatched through the registry
    /// runs the subprocess, returns the result, and writes the audit
    /// `python_executed` event. This is the same path the LLM takes:
    /// `parse_markers → dispatch_marker → registry.dispatch →
    /// RunPythonTool.call → run_python_code`. The call is synchronous-
    /// blocking; the future resolves only after the subprocess exits.
    #[tokio::test]
    async fn dispatch_marker_run_python_executes_subprocess_and_logs_audit() {
        use crate::registry::ToolRegistry;
        use crate::tool::builtin_tools;
        let mem = Memory::new(db::open_memory().await.unwrap());
        let mut reg = ToolRegistry::new();
        reg.register_all(builtin_tools());
        let marker = crate::skill::Marker {
            op: "run_python".into(),
            payload: serde_json::json!({"code": "print(7 * 6)"}),
        };
        dispatch_marker(
            &mem, mem.bus(), &reg, "r-py-e2e",
            AGENTS.iter().find(|a| a.name == "generation").unwrap(),
            &marker, 0,
        )
        .await
        .expect("dispatch ok");

        // Audit event should be written by the tool's log_event call.
        let py_count = mem
            .conn()
            .query(
                "SELECT COUNT(*) FROM events WHERE run_id = ?1 AND type = 'python_executed'",
                ["r-py-e2e"],
            )
            .await
            .unwrap()
            .next()
            .await
            .unwrap()
            .map(|r| r.get::<i64>(0).unwrap())
            .unwrap_or(0);
        assert_eq!(py_count, 1, "exactly one python_executed event row");
    }

    /// The prompt cache must invalidate on a `SemanticSaved` event
    /// even when the write did not flow through `dispatch_marker`.
    /// Previously the doc-comment promised this, but the bus
    /// subscription was never instantiated — only the dispatcher's
    /// own success path invalidated. This test pins the new contract:
    /// the consolidation service (or any other producer) publishes
    /// `SemanticSaved`, and the cache entry vanishes.
    #[tokio::test]
    async fn prompt_cache_invalidates_on_external_semantic_saved() {
        let mem = Memory::new(db::open_memory().await.unwrap());
        let cache = Arc::new(PromptContextCache::new());
        let bus = mem.bus().clone();
        let _drain = cache.attach_bus(bus.clone(), "external-run".to_string());

        // Populate the cache.
        cache.put(CachedPromptBlocks {
            prior_behavior: vec![],
            prior_session: "old".into(),
            marker_errors: vec![],
            step_index: 0,
        });
        assert_eq!(cache._len(), 1, "cache populated");

        // Publish from "outside" — no dispatch_marker involved.
        // The run_id matches; this should invalidate.
        bus.publish(crate::bus::MemoryEvent::SemanticSaved {
            id: 1,
            run_id: "external-run".into(),
            scope: "experiment".into(),
            summary: "from consolidation".into(),
        });
        // Give the drain task a moment to receive.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(cache._len(), 0, "cache must invalidate on SemanticSaved");
    }

    /// A `SemanticSaved` from a *different* run must NOT invalidate
    /// this cache. The drain task filters on `run_id`.
    #[tokio::test]
    async fn prompt_cache_ignores_semantic_saved_from_other_run() {
        let mem = Memory::new(db::open_memory().await.unwrap());
        let cache = Arc::new(PromptContextCache::new());
        let bus = mem.bus().clone();
        let _drain = cache.attach_bus(bus.clone(), "this-run".to_string());

        cache.put(CachedPromptBlocks {
            prior_behavior: vec![],
            prior_session: "old".into(),
            marker_errors: vec![],
            step_index: 0,
        });
        assert_eq!(cache._len(), 1);

        // Different run_id — should be ignored.
        bus.publish(crate::bus::MemoryEvent::SemanticSaved {
            id: 1,
            run_id: "other-run".into(),
            scope: "experiment".into(),
            summary: "not mine".into(),
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            cache._len(),
            1,
            "cache must NOT invalidate on other-run SemanticSaved"
        );

        // Same run_id — should invalidate.
        bus.publish(crate::bus::MemoryEvent::SemanticSaved {
            id: 2,
            run_id: "this-run".into(),
            scope: "experiment".into(),
            summary: "mine".into(),
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(cache._len(), 0, "cache must invalidate on this-run SemanticSaved");
    }
}
