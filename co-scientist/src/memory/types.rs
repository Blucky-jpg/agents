//! Pure types shared across the memory module: no SQL, no async.
//!
//! These are the value objects that callers construct and consume.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: i64,
    pub run_id: String,
    pub agent_id: i64,
    pub step_index: i64,
    pub r#type: String,
    pub payload: Option<Value>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticMemory {
    pub id: i64,
    pub run_id: String,
    pub agent_id: Option<i64>,
    pub scope: String,
    pub summary: String,
    pub details: Option<Value>,
    pub importance: f64,
    pub archived: bool,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BehaviorMemory {
    pub id: i64,
    pub agent_id: i64,
    pub pattern: String,
    pub notes: String,
    pub evidence: Option<Value>,
    pub created_at: String,
}

/// What `get_context` returns. Ready to be rendered into a prompt.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Context {
    pub recent_events: Vec<Event>,
    pub semantic: Vec<SemanticMemory>,
    pub behavior: Vec<BehaviorMemory>,
    /// A pre-formatted string the caller can drop into a user message.
    pub rendered: String,
    /// Approximate token count of `rendered` (4 chars/token heuristic).
    /// `0` if the field is empty.
    pub tokens_approx: usize,
}

/// A compact peek returned by [`super::Memory::peek_context`]. Layer 1 of the
/// 3-layer retrieval pattern: scan many candidates cheaply, then
/// fetch full detail only for the relevant ones.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeekedMemory {
    pub id: i64,
    pub kind: PeekedKind,
    /// One-line summary suitable for the model to scan in 5 seconds.
    pub summary: String,
    /// Scope for semantic memories; pattern for behavior memories.
    pub label: String,
    /// Approximate token cost of the full detail.
    pub tokens_approx: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeekedKind {
    Semantic,
    Behavior,
}

#[derive(Debug, Clone)]
pub struct ContextLimits {
    pub events: usize,
    pub semantic: usize,
    pub behavior: usize,
    /// Maximum approximate tokens for the rendered context. 0 = unlimited.
    /// When set, the rendered output is truncated to fit within this
    /// budget (priority: semantic > behavior > events).
    pub max_tokens: usize,
    /// How many of the most recent semantic memories get full detail
    /// (summary + details JSON) in the rendered context. The rest show
    /// as compact one-liners: `id [scope] summary`. 0 = all compact.
    pub full_count: usize,
}

impl Default for ContextLimits {
    fn default() -> Self {
        Self {
            events: 20,
            semantic: 8,
            behavior: 5,
            max_tokens: 0,
            full_count: 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObservationKind {
    Semantic,
    Behavior,
}

/// Layer 3 result: the full row for one observation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Observation {
    Semantic(SemanticMemory),
    Behavior(BehaviorMemory),
}