//! `agents` table operations: lookup, upsert, ensure-create.

use anyhow::{Context as _, Result};
use chrono::Utc;

use super::{Memory, MemoryError};

impl Memory {
    /// Look up the integer id for an agent name. Inserts a placeholder row
    /// (with empty system_prompt) if missing — callers that care about the
    /// prompt should seed via [`Memory::upsert_agent`] instead.
    pub async fn ensure_agent(&self, name: &str) -> Result<i64, MemoryError> {
        if let Some(id) = self.agent_id(name).await? {
            return Ok(id);
        }
        self.db
            .conn()
            .execute(
                "INSERT INTO agents (name, role, system_prompt, created_at) VALUES (?1, '', '', ?2)",
                (name, Utc::now().to_rfc3339()),
            )
            .await
            .context("inserting placeholder agent")?;
        self.agent_id(name)
            .await?
            .ok_or_else(|| MemoryError::AgentNotFound(name.to_string()))
    }

    /// Upsert an agent with full prompt/role. Idempotent by `name`.
    pub async fn upsert_agent(
        &self,
        name: &str,
        role: &str,
        system_prompt: &str,
    ) -> Result<i64, MemoryError> {
        let now = Utc::now().to_rfc3339();
        self.db
            .conn()
            .execute(
                "INSERT INTO agents (name, role, system_prompt, created_at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(name) DO UPDATE SET role = excluded.role, system_prompt = excluded.system_prompt",
                (name, role, system_prompt, now),
            )
            .await
            .context("upserting agent")?;
        self.agent_id(name)
            .await?
            .ok_or_else(|| MemoryError::AgentNotFound(name.to_string()))
    }

    pub(super) async fn agent_id(&self, name: &str) -> Result<Option<i64>, MemoryError> {
        let mut rows = self
            .conn()
            .query("SELECT id FROM agents WHERE name = ?1", [name])
            .await
            .context("querying agent id")?;
        if let Some(row) = rows.next().await? {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }

    /// Test-only helper to peek an agent id without taking `&self` mutably.
    /// Gated so it doesn't appear in the public API in production builds.
    #[cfg(any(test, feature = "test-helpers"))]
    #[doc(hidden)]
    pub async fn agent_id_for_test(&self, name: &str) -> Result<Option<i64>, MemoryError> {
        self.agent_id(name).await
    }
}