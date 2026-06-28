//! Co-scientist: a local, in-process memory layer for an ante-driven multi-agent
//! research loop.
//!
//! **Design contract**
//!
//! - This crate does NOT modify any ante code. It uses an in-tree
//!   `claude_cli::ClaudeCli` client that wraps the `claude` CLI subprocess;
//!   everything else (the loop, the skill protocol, the memory API) lives
//!   in this crate.
//! - The "agent loop" is owned by [`runner::Runner`], not by ante. We call
//!   `claude.query(prompt)` as a turn primitive and dispatch memory ops
//!   ourselves based on markers the model emits.
//! - All state is in a local `co_scientist.db` (Turso/libSQL in embedded mode).
//!   No HTTP, no daemon, no network unless you bolt on an embedding API later.
//!
//! **Integration model used (no fork)**
//!
//! - Hooks → we don't get true inside-step hooks from outside. The closest
//!   equivalent is a *turn-boundary* hook: `Runner` calls `Memory::log_event`
//!   before and after every `Claude::query` call.
//! - Plugins → we register 3 "virtual" tools by parsing structured markers
//!   out of the model's text response (`[[MEMORY_OP:save_semantic:{...}]]`).
//!   See [`skill`] for the format.
//! - Skills → the LLM-facing instructions live in `SKILL.md`, embedded as a
//!   `&'static str` via `include_str!`. Loaded into the system prompt.

pub mod agents;
pub mod bus;
pub mod claude_cli;
pub mod db;
pub mod elo;
pub mod embeddings;
pub mod experiment;
pub mod llm_query;
pub mod hypothesis;
pub mod marker_normalizer;
pub mod memory;
pub mod policies;
pub mod prompts;
pub mod prompt_allowlist;
pub mod promotion;
pub mod queue;
pub mod registry;
pub mod research_session;
pub mod run_agent;
pub mod runner;
pub mod skill;
pub mod skill_loader;
pub mod supervisor;
pub mod supervisor_bundle;
pub mod tool;
pub mod tools;
pub mod tournament;
pub mod worker;

pub use agents::Agent;
pub use bus::{
    run_failure_aggregator, EventBus, FailureAggregatorConfig, FailureCount, MemoryEvent,
};
pub use experiment::{Experiment, ExperimentRepo, ExperimentStatus, RunResult};
pub use db::{open, Db};
pub use memory::{
    cite, approx_tokens, Context, Event, Memory, MemoryError, Observation, ObservationKind,
    PeekedKind, PeekedMemory, SemanticMemory,
};
pub use marker_normalizer::{canonicalize, derive_summary, normalize, ToolAlias};
pub use llm_query::{is_transient_anyhow, is_transient_error, jitter};
pub use prompts::{AgentMode, Prompts, PromptContext, PROMPT_MODES};
pub use promotion::{ConsolidationService, PromotionConfig};
pub use queue::{EnqueueRequest, Task, TaskQueue, TaskStatus};
pub use registry::{default_allowlist, ToolRegistry};
pub use run_agent::{RunAgentTool, SessionRunners};
pub use runner::{Runner, RunnerConfig};
pub use skill::{parse_markers, Marker, SKILL};
pub use skill_loader::{discover as discover_skills, into_tool as skill_to_tool, LoadedSkill};
pub use supervisor::{Supervisor, SupervisorConfig};
pub use tool::{
    builtin_tools, CompressEventsTool, GetContextTool, GetObservationTool, GetTimelineTool,
    PeekContextTool, RecordHypothesisTool, RecordReviewTool, RecordTournamentMatchTool,
    SaveBehaviorTool, SaveSemanticTool, Tool, ToolCtx, ToolOutput,
};
pub use worker::{ctrl_c_shutdown, enqueue_memory_op, run_worker, WorkerConfig};
