//! `behavior_memories` table operations: save/search/peek/recent/observe/archive/delete.

use anyhow::{Context as _, Result};
use chrono::Utc;
use serde_json::Value;

use super::{
    helpers::{idempotency_key, tokenize},
    types::{BehaviorMemory, Observation, ObservationKind, PeekedKind, PeekedMemory},
    Memory, MemoryError,
};
use crate::bus::MemoryEvent;
use crate::db::Rows;

impl Memory {
    pub async fn save_behavior(
        &self,
        agent_name: &str,
        pattern: &str,
        notes: &str,
        evidence: Option<Value>,
    ) -> Result<i64, MemoryError> {
        let agent_id = self.ensure_agent(agent_name).await?;
        let evidence_str = evidence
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .context("serializing evidence")?;
        let key = idempotency_key(&[
            "behavior",
            &agent_id.to_string(),
            pattern,
            notes,
            evidence_str.as_deref().unwrap_or(""),
        ]);
        let mut rows = self
            .conn()
            .query(
                "INSERT INTO behavior_memories (agent_id, pattern, notes, evidence_json, created_at, idempotency_key)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(idempotency_key) DO NOTHING
                 RETURNING id",
                (
                    agent_id,
                    pattern,
                    notes,
                    evidence_str,
                    Utc::now().to_rfc3339(),
                    key.clone(),
                ),
            )
            .await
            .context("inserting behavior memory")?;
        if let Some(row) = rows.next().await? {
            let id: i64 = row.get(0)?;
            self.bus.publish(MemoryEvent::BehaviorSaved {
                id,
                agent: agent_name.to_string(),
                pattern: pattern.to_string(),
            });
            self.reindex_behavior(id, pattern, notes).await?;
            return Ok(id);
        }
        // Conflict: look up the existing id.
        let mut rows = self
            .conn()
            .query(
                "SELECT id FROM behavior_memories WHERE idempotency_key = ?1",
                [key],
            )
            .await
            .context("looking up behavior by idempotency key")?;
        if let Some(row) = rows.next().await? {
            Ok(row.get(0)?)
        } else {
            Err(MemoryError::Other(
                "behavior insert returned no row but row was not found".into(),
            ))
        }
    }

    /// Latest N behavior notes for one agent, newest first. Used by the
    /// runner to inject prior self-criticism into the system prompt.
    pub async fn recent_behavior(
        &self,
        agent_name: &str,
        limit: usize,
    ) -> Result<Vec<BehaviorMemory>, MemoryError> {
        let agent_id = self.ensure_agent(agent_name).await?;
        let sql = "SELECT id, agent_id, pattern, notes, evidence_json, created_at
                   FROM behavior_memories
                   WHERE agent_id = ?1
                   ORDER BY id DESC
                   LIMIT ?2";
        let mut rows = self.db.conn().query(sql, (agent_id, limit as i64)).await?;
        self.collect_behavior_rows(&mut rows).await
    }

    /// Load recent system feedback from behavior_memories (across all agents).
    /// Returns the `notes` field of the most recent feedback entries.
    pub async fn recent_system_feedback(&self, limit: usize) -> Vec<String> {
        let result = self
            .conn()
            .query(
                "SELECT notes FROM behavior_memories
                 WHERE pattern LIKE '%system_feedback%' OR pattern LIKE '%meta%'
                 ORDER BY id DESC LIMIT ?1",
                [limit as i64],
            )
            .await;
        let mut rows = match result {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.unwrap_or(None) {
            if let Ok(notes) = row.get::<String>(0) {
                out.push(notes);
            }
        }
        out
    }

    /// Inverted-index search over `behavior_memories` for `query`,
    /// scoped to `agent_id`. Returns full [`BehaviorMemory`] rows,
    /// ranked by term overlap.
    pub async fn search_behavior(
        &self,
        agent_id: i64,
        query: &str,
        limit: usize,
    ) -> Result<Vec<BehaviorMemory>, MemoryError> {
        let q = query.trim();
        if q.is_empty() {
            let mut rows = self
                .conn()
                .query(
                    "SELECT id, agent_id, pattern, notes, evidence_json, created_at
                     FROM behavior_memories
                     WHERE agent_id = ?1 AND archived = 0
                     ORDER BY id DESC
                     LIMIT ?2",
                    (agent_id, limit as i64),
                )
                .await?;
            return self.collect_behavior_rows(&mut rows).await;
        }
        let terms = tokenize(q);
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let term_list = terms
            .iter()
            .map(|t| format!("'{t}'"))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT memory_id, COUNT(*) FROM term_index
             WHERE memory_kind = 'behavior' AND term IN ({term_list})
               AND memory_id IN (SELECT id FROM behavior_memories WHERE agent_id = ?1 AND archived = 0)
             GROUP BY memory_id"
        );
        let mut scores: std::collections::HashMap<i64, f64> = std::collections::HashMap::new();
        let mut rows = self.db.conn().query(&sql, [agent_id]).await?;
        while let Some(row) = rows.next().await? {
            let id: i64 = row.get(0)?;
            let count: i64 = row.get(1)?;
            scores.insert(id, count as f64);
        }
        let mut ids: Vec<(i64, f64)> = scores.into_iter().collect();
        ids.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        ids.truncate(limit);
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        // Inline the IDs (see note in `search_semantic`).
        let id_list = ids
            .iter()
            .map(|(id, _)| id.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT id, agent_id, pattern, notes, evidence_json, created_at
             FROM behavior_memories
             WHERE id IN ({id_list}) AND archived = 0"
        );
        let mut rows = self.db.conn().query(&sql, ()).await?;
        let mut results = self.collect_behavior_rows(&mut rows).await?;
        let id_to_score: std::collections::HashMap<i64, f64> = ids.into_iter().collect();
        results.sort_by(|a, b| {
            let sa = id_to_score.get(&a.id).copied().unwrap_or(0.0);
            let sb = id_to_score.get(&b.id).copied().unwrap_or(0.0);
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(results)
    }

    pub(super) async fn collect_behavior_rows(
        &self,
        rows: &mut Rows,
    ) -> Result<Vec<BehaviorMemory>, MemoryError> {
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            let evidence_str: Option<String> = row.get(4)?;
            let evidence = evidence_str
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .context("parsing evidence")?;
            out.push(BehaviorMemory {
                id: row.get(0)?,
                agent_id: row.get(1)?,
                pattern: row.get(2)?,
                notes: row.get(3)?,
                evidence,
                created_at: row.get(5)?,
            });
        }
        for m in &out {
            self.bump_last_accessed("behavior", m.id).await;
        }
        Ok(out)
    }

    pub(super) async fn peek_behavior(
        &self,
        agent_name: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<PeekedMemory>, MemoryError> {
        let agent_id = self.ensure_agent(agent_name).await?;
        let mems = self.search_behavior(agent_id, query, limit).await?;
        Ok(mems
            .into_iter()
            .map(|m| PeekedMemory {
                id: m.id,
                kind: PeekedKind::Behavior,
                summary: m.notes,
                label: m.pattern,
                tokens_approx: 0,
            })
            .collect())
    }

    pub(super) async fn get_behavior_observation(
        &self,
        id: i64,
    ) -> Result<Option<Observation>, MemoryError> {
        let mut rows = self
            .conn()
            .query(
                "SELECT id, agent_id, pattern, notes, evidence_json, created_at
                 FROM behavior_memories WHERE id = ?1 AND archived = 0",
                [id],
            )
            .await
            .context("querying behavior by id")?;
        if let Some(row) = rows.next().await? {
            let evidence_str: Option<String> = row.get(4)?;
            let evidence = evidence_str
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .context("parsing evidence")?;
            Ok(Some(Observation::Behavior(BehaviorMemory {
                id: row.get(0)?,
                agent_id: row.get(1)?,
                pattern: row.get(2)?,
                notes: row.get(3)?,
                evidence,
                created_at: row.get(5)?,
            })))
        } else {
            Ok(None)
        }
    }

    pub(super) async fn reindex_behavior(
        &self,
        id: i64,
        pattern: &str,
        notes: &str,
    ) -> Result<(), MemoryError> {
        self.db
            .conn()
            .execute(
                "DELETE FROM term_index WHERE memory_kind = 'behavior' AND memory_id = ?1",
                [id],
            )
            .await
            .context("clearing behavior term_index")?;
        let terms = tokenize(&format!("{pattern} {notes}"));
        for term in terms {
            self.db
                .conn()
                .execute(
                    "INSERT OR IGNORE INTO term_index (memory_kind, memory_id, term) VALUES ('behavior', ?1, ?2)",
                    (id, term),
                )
                .await
                .context("indexing behavior term")?;
        }
        Ok(())
    }

    /// Archive a behavior memory (set archived = 1).
    pub async fn archive_behavior(&self, id: i64) -> Result<(), MemoryError> {
        self.db
            .conn()
            .execute(
                "UPDATE behavior_memories SET archived = 1 WHERE id = ?1",
                [id],
            )
            .await
            .context("archiving behavior memory")?;
        Ok(())
    }

    /// Hard-delete a behavior memory. Caller is expected to have
    /// recorded audit `evidence` (the DeleteObservationTool enforces
    /// this). Returns the number of rows removed (0 = nothing to delete,
    /// or already gone).
    pub async fn delete_behavior(&self, id: i64) -> Result<usize, MemoryError> {
        let n = self
            .db
            .conn()
            .execute("DELETE FROM behavior_memories WHERE id = ?1", [id])
            .await
            .context("deleting behavior memory")?;
        Ok(n as usize)
    }

    /// Look up `(agent_name, created_at)` for a behavior observation. Used
    /// by [`super::super::context::get_timeline`].
    pub(super) async fn behavior_timeline_key(
        &self,
        observation_id: i64,
    ) -> Result<Option<String>, MemoryError> {
        let mut rows = self
            .conn()
            .query(
                "SELECT created_at FROM behavior_memories WHERE id = ?1",
                [observation_id],
            )
            .await?;
        if let Some(row) = rows.next().await? {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }

    /// Suppress unused-import warning when `ObservationKind` is referenced
    /// only through `impl` return types in trait-adjacent contexts.
    #[allow(dead_code)]
    fn _kind_behavior() -> ObservationKind {
        ObservationKind::Behavior
    }
}