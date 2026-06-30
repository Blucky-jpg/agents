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

// =====================================================================
// Categorized modules. Each bucket holds a coherent slice of the system.
// See CONTEXT.md for the domain glossary.
// =====================================================================
pub mod agent_loop;
pub mod agents;
pub mod lifecycle;
pub mod llm_io;
pub mod marker;
pub mod memory;
pub mod research;
pub mod tool_catalog;
pub mod tools;
pub mod tournament;

// =====================================================================
// Flat re-exports — preserve the historical top-level surface so existing
// `use crate::xxx::...` paths keep compiling, and so the CLI's imports stay
// stable across module reshuffles. New code should prefer the bucket paths.
// =====================================================================
pub use crate::agents::Agent;
pub use crate::agent_loop::runner::{Runner, RunnerConfig};
pub use crate::agent_loop::run_agent::{RunAgentTool, SessionRunners};
pub use crate::lifecycle::bus::{
    run_failure_aggregator, EventBus, FailureAggregatorConfig, FailureCount, MemoryEvent,
};
pub use crate::lifecycle::promotion::{ConsolidationService, PromotionConfig};
pub use crate::lifecycle::queue::{EnqueueRequest, Task, TaskQueue, TaskStatus};
pub use crate::lifecycle::supervisor::{Supervisor, SupervisorConfig};
pub use crate::lifecycle::supervisor_bundle::{
    self as supervisor_bundle, BundleOutcome, Config as BundleConfig,
};
pub use crate::lifecycle::worker::{ctrl_c_shutdown, run_worker, WorkerConfig};
pub use crate::llm_io::llm_query::{is_transient_anyhow, is_transient_error, jitter};
pub use crate::llm_io::prompts::{AgentMode, Prompts, PromptContext, PROMPT_MODES};
pub use crate::llm_io::skill_loader::{
    discover as discover_skills, into_tool as skill_to_tool, LoadedSkill,
};
pub use crate::marker::normalizer::{canonicalize, derive_summary, normalize, ToolAlias};
pub use crate::marker::skill::{parse_markers, Marker, SKILL};
pub use crate::memory::db::{open, Db};
pub use crate::memory::{
    cite, approx_tokens, Context, Event, Memory, MemoryError, Observation, ObservationKind,
    PeekedKind, PeekedMemory, SemanticMemory,
};
pub use crate::memory::research_session::ResearchSessionRepo;
pub use crate::research::experiment::{Experiment, ExperimentRepo, ExperimentStatus, RunResult};
pub use crate::tools::{
    builtin_tools, ArchiveObservationTool, CompressEventsTool, DeleteObservationTool,
    GetContextTool, GetObservationTool, GetTimelineTool, PeekContextTool, RecordHypothesisTool,
    RecordReviewTool, RecordTournamentMatchTool, SaveBehaviorTool, SaveSemanticTool, Tool, ToolCtx,
    ToolOutput,
};
pub use crate::tools::registry::{default_allowlist, ToolRegistry};
pub use crate::tournament::elo::{expected_score, update_elo, Winner};
pub use crate::tournament::hypothesis::{Hypothesis, HypothesisRepo, HypothesisState};
pub use crate::tournament::matches::{TournamentMatch, TournamentRepo};

// =====================================================================
// Backwards-compat shims — keep the old top-level module names so existing
// `use crate::xxx::...` paths inside this crate keep compiling, and so
// downstream code that reached for `co_scientist::elo::Winner` etc. still
// resolves to the same item.
// =====================================================================
pub mod bus {
    pub use crate::lifecycle::bus::*;
}
pub mod claude_cli {
    pub use crate::llm_io::claude_cli::*;
}
pub mod db {
    pub use crate::memory::db::*;
}
pub mod elo {
    pub use crate::tournament::elo::*;
}
pub mod embeddings {
    pub use crate::llm_io::embeddings::*;
}
pub mod experiment {
    pub use crate::research::experiment::*;
}
pub mod hypothesis {
    pub use crate::tournament::hypothesis::*;
}
pub mod llm_query {
    pub use crate::llm_io::llm_query::*;
}
pub mod marker_normalizer {
    pub use crate::marker::normalizer::*;
}
pub mod policies {
    pub use crate::lifecycle::policies::*;
}
pub mod promotion {
    pub use crate::lifecycle::promotion::*;
}
pub mod prompt_allowlist {
    pub use crate::marker::allowlist::*;
}
pub mod prompts {
    pub use crate::llm_io::prompts::*;
}
pub mod queue {
    pub use crate::lifecycle::queue::*;
}
pub mod registry {
    pub use crate::tools::registry::*;
}
pub mod research_session {
    pub use crate::memory::research_session::*;
}
pub mod runner {
    pub use crate::agent_loop::runner::*;
}
pub mod run_agent {
    pub use crate::agent_loop::run_agent::*;
}
pub mod skill {
    pub use crate::marker::skill::*;
}
pub mod skill_loader {
    pub use crate::llm_io::skill_loader::*;
}
pub mod supervisor {
    pub use crate::lifecycle::supervisor::*;
}
pub mod tool {
    pub use crate::tools::*;
}
pub mod worker {
    pub use crate::lifecycle::worker::*;
}