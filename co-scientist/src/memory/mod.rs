//! Memory API: pure functions over a [`Db`] handle.
//!
//! - [`Memory::log_event`]      → insert into `events`
//! - [`Memory::save_semantic`]  → insert into `semantic_memories`
//! - [`Memory::save_behavior`]  → insert into `behavior_memories`
//! - [`Memory::get_context`]    → fetch last N events + top-K semantic + behavior notes
//!
//! All structural fields (`run_id`, `agent_id`, `step_index`, timestamps) are
//! written by us, not the LLM. The LLM only chooses *what* to save, never
//! *how* to identify it in the DB.
//!
//! ## Module layout
//!
//! The 1773-line file that used to live at `src/memory.rs` was split so that
//! each table or concern lives in its own file:
//!
//! | file           | contents                                                  |
//! |----------------|-----------------------------------------------------------|
//! | `types.rs`     | Pure value types: `Event`, `SemanticMemory`, `BehaviorMemory`, `Context`, etc. |
//! | `helpers.rs`   | Pure helpers: `tokenize`, `idempotency_key`, `cite`, `approx_tokens`, `render_context`, porter stemmer. |
//! | `events.rs`    | `Memory` impl for the `events` + `sessions` tables.        |
//! | `agents.rs`    | `Memory` impl for the `agents` table.                      |
//! | `semantic.rs`  | `Memory` impl for the `semantic_memories` table + indexing. |
//! | `behavior.rs`  | `Memory` impl for the `behavior_memories` table + indexing. |
//! | `context.rs`   | `Memory` impl for the 3-layer retrieval pattern.           |
//!
//! Every item that was public at the top level of the old `memory.rs` is
//! re-exported here so external callers don't need to change.

mod agents;
mod behavior;
mod context;
mod events;
mod helpers;
mod semantic;
mod types;

use std::sync::Arc;

use anyhow::Result;
use thiserror::Error;

use crate::bus::{EventBus, MemoryEvent};
use crate::db::Db;

// ---- Public re-exports (back-compat with the old memory.rs API) ----

pub use helpers::{approx_tokens, cite, idempotency_key, new_run_id, tokenize};
pub use types::{
    BehaviorMemory, Context, ContextLimits, Event, Observation, ObservationKind, PeekedKind,
    PeekedMemory, SemanticMemory,
};

// ---- Error type ----

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("agent not found: {0}")]
    AgentNotFound(String),
    #[error("db error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// Lossy conversion from `anyhow::Error` (used by call sites that
    /// chain `.context("...")?`). The original error chain is dropped
    /// here; callers that need the chain should return
    /// `anyhow::Result<T>` at the boundary.
    #[error("{0}")]
    Other(String),
}

impl From<anyhow::Error> for MemoryError {
    fn from(e: anyhow::Error) -> Self {
        Self::Other(format!("{e:#}"))
    }
}

// ---- Core handle ----

/// Cheaply-cloneable handle to the memory layer. Wraps an `Arc<Db>` and
/// an `EventBus` so it can be passed into a turn loop and across awaits
/// without ceremony.
#[derive(Clone)]
pub struct Memory {
    pub(crate) db: Arc<Db>,
    pub(crate) bus: EventBus,
}

impl Memory {
    /// Create a Memory handle from an existing Db. The provided Db's
    /// connection becomes THIS Memory's connection. To get a Memory
    /// with its own connection, use `Memory::with_fresh_conn(db)` or
    /// `Memory::with_fresh_conn_async(db)` for async DBs.
    pub fn new(db: Db) -> Self {
        Self {
            db: Arc::new(db),
            bus: EventBus::default(),
        }
    }

    /// Create a Memory sharing a specific bus.
    pub fn with_bus(db: Db, bus: EventBus) -> Self {
        Self {
            db: Arc::new(db),
            bus,
        }
    }

    /// Create a Memory with its own fresh connection. Requires the Db
    /// to have been opened with `Db::open_with_async_conn`. Returns
    /// a new Memory (and Db behind it) that shares the same file but
    /// has an independent connection handle.
    pub fn with_fresh_conn(db: &Db, new_conn: rusqlite::Connection) -> Self {
        let new_db = db.with_new_conn(new_conn);
        Self::new(new_db)
    }

    /// Get the per-Memory DB connection. Each Memory holds its own
    /// connection, so concurrent Memory users don't fight over a single
    /// connection handle.
    pub fn conn(&self) -> crate::db::Conn {
        self.db.conn()
    }

    /// Open a NEW connection to the same DB file and return a new Db.
    /// Each Memory clone should call this once so writes don't serialize.
    pub async fn new_db_with_fresh_conn(&self, path: &str) -> Result<crate::db::Db> {
        let conn = crate::db::Db::connect_fresh(path).await?;
        Ok(crate::db::Db::new(conn))
    }

    /// Subscribe to the live event tail. See [`crate::bus::EventBus`].
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<MemoryEvent> {
        self.bus.subscribe()
    }

    /// Clone the event bus handle. Used by the consolidation service
    /// to subscribe to live events.
    pub fn bus(&self) -> &EventBus {
        &self.bus
    }

    /// CLI-only accessor for row counts and ad-hoc inspection. The `pub`
    /// here is a thin escape hatch — the public memory API stays narrow.
    pub fn db(&self) -> &Db {
        &self.db
    }

    /// Clone the Arc<Db> for use in repos that need to own the handle.
    pub fn db_arc(&self) -> Arc<Db> {
        self.db.clone()
    }

    /// Bump `last_accessed_at` for one observation. Called by retrieval
    /// paths (search_semantic / peek_semantic / search_behavior /
    /// peek_behavior) so the consolidation service can decay unused
    /// memories. Errors are swallowed — losing the timestamp is not
    /// worth failing a read.
    pub async fn bump_last_accessed(&self, kind: &str, id: i64) {
        let table = match kind {
            "semantic" => "semantic_memories",
            "behavior" => "behavior_memories",
            _ => return,
        };
        let sql = format!("UPDATE {table} SET last_accessed_at = ?1 WHERE id = ?2");
        let now = chrono::Utc::now().to_rfc3339();
        let _ = self.db.conn().execute(&sql, (now, id)).await;
    }

    /// Back-compat shim: the old API exposed `tokenize` as an associated
    /// function. It now lives in [`helpers::tokenize`]; this forwards so
    /// external callers (e.g. `promotion.rs`) keep working unchanged.
    pub fn tokenize(text: &str) -> Vec<String> {
        helpers::tokenize(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The associated `Memory::tokenize` is a shim over `helpers::tokenize`.
    /// Pin this so a future refactor can't silently diverge them and break
    /// `promotion.rs:342` (which uses the associated form).
    #[test]
    fn memory_tokenize_shim_matches_helpers_tokenize() {
        for input in [
            "",
            "KRAS-G12C binds sotorasib",
            "the a an of",
            "experiments show experimental results",
        ] {
            assert_eq!(
                Memory::tokenize(input),
                helpers::tokenize(input),
                "shim diverged from helpers::tokenize for input {input:?}"
            );
        }
    }
}