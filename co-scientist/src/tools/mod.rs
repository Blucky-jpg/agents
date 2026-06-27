//! Tool plugin contract.
//!
//! A [`Tool`] is a typed callable that the LLM (or a worker) can invoke by
//! name. Each tool declares a JSONSchema for its arguments, a description,
//! and an async `call` that returns a `serde_json::Value`.
//!
//! The model never calls tools directly in this crate — the runner
//! receives tool invocations from the LLM (or, for now, from the legacy
//! `[[MEMORY_OP:...]]` marker format and the task queue) and dispatches
//! them through a [`ToolRegistry`].
//!
//! This replaces the fragile text-marker parser in `skill.rs`. The old
//! `parse_markers` still works for backward compatibility and as a
//! fallback, but new code should use the registry.
//!
//! Concrete tools live in the [`memory`], [`research`], and [`curation`]
//! sub-modules. They are re-exported below so callers can continue to
//! write `crate::tool::SaveSemanticTool`.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::memory::Memory;

mod curation;
mod memory;
mod research;

pub use curation::*;
pub use memory::*;
pub use research::*;

/// Per-call context passed to a tool. Carries the memory handle, run id,
/// and the calling agent's name. Tools are long-lived; the registry
/// dispatches one `ToolCtx` per call.
#[derive(Clone)]
pub struct ToolCtx {
    pub memory: Memory,
    pub run_id: String,
    pub agent_name: String,
}

impl std::fmt::Debug for ToolCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCtx")
            .field("run_id", &self.run_id)
            .field("agent_name", &self.agent_name)
            .finish()
    }
}

/// What a tool returns to the caller. `Value` is a `serde_json::Value` so
/// the same shape can be serialized to the LLM, printed to a log, or
/// stored.
pub type ToolOutput = Value;

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> String;
    fn input_schema(&self) -> Value;

    /// Execute. `args` is guaranteed by the registry to match
    /// `input_schema` (registry should validate before calling).
    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput>;
}

/// The standard set of memory tools, ready to register.
pub fn builtin_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(SaveSemanticTool),
        Arc::new(SaveBehaviorTool),
        Arc::new(GetContextTool),
        Arc::new(CompressEventsTool),
        Arc::new(PeekContextTool),
        Arc::new(GetTimelineTool),
        Arc::new(GetObservationTool),
        Arc::new(ArchiveObservationTool),
        Arc::new(DeleteObservationTool),
        Arc::new(RecordHypothesisTool),
        Arc::new(RecordReviewTool),
        Arc::new(RecordTournamentMatchTool),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::memory::Memory;

    #[tokio::test]
    async fn save_semantic_tool_round_trip() {
        let mem = Memory::new(db::open_memory().await.unwrap());
        let ctx = ToolCtx {
            memory: mem.clone(),
            run_id: "r1".into(),
            agent_name: "hypothesis".into(),
        };
        let tool = SaveSemanticTool;
        let out = tool
            .call(
                serde_json::json!({"scope": "insight", "summary": "x > y", "details": {"k": 1}}),
                &ctx,
            )
            .await
            .unwrap();
        let id = out["id"].as_i64().unwrap();
        assert!(id > 0);
    }

    #[test]
    fn schemas_are_valid_json_schema_objects() {
        for t in builtin_tools() {
            let s = t.input_schema();
            assert_eq!(s["type"], "object", "{}: schema must declare object", t.name());
        }
    }
}