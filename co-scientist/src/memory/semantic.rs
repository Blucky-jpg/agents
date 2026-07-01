//! `semantic_memories` table operations: save/search/peek/observe/archive,
//! plus the inverted-index maintenance that powers full-text retrieval.
//!
//! `find_near_duplicate` is the only embedding-touching code here; it stays
//! inside the save path because dedup has to happen *before* insertion.

use anyhow::{Context as _, Result};
use chrono::Utc;
use serde_json::Value;

use super::{
    helpers::{idempotency_key, tokenize},
    types::{Observation, ObservationKind, PeekedKind, PeekedMemory, SemanticMemory},
    Memory, MemoryError,
};
use crate::bus::MemoryEvent;
use crate::db::Rows;

impl Memory {
    /// Idempotent on `(run_id, agent_name, scope, summary, sha256(details))`.
    /// Also checks for near-duplicates via cosine similarity (threshold 0.92)
    /// against existing non-archived semantic memories. Returns the row id,
    /// whether newly inserted, pre-existing by key, or pre-existing by dedup.
    pub async fn save_semantic(
        &self,
        run_id: &str,
        agent_name: Option<&str>,
        scope: &str,
        summary: &str,
        details: Option<Value>,
    ) -> Result<i64, MemoryError> {
        let agent_id = match agent_name {
            Some(name) => Some(self.ensure_agent(name).await?),
            None => None,
        };
        let details_str = details
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .context("serializing details")?;
        let key = idempotency_key(&[
            "semantic",
            run_id,
            agent_id.map(|i| i.to_string()).as_deref().unwrap_or(""),
            scope,
            summary,
            details_str.as_deref().unwrap_or(""),
        ]);

        // Layer 2 dedup: check cosine similarity against existing memories.
        let embed_text = format!("{} {}", summary, details_str.as_deref().unwrap_or(""));
        let new_vec = crate::embeddings::hash_bag(&embed_text, crate::embeddings::HASH_DIM);
        if let Some(dup_id) = self.find_near_duplicate(&new_vec, 0.92).await? {
            return Ok(dup_id);
        }

        // ON CONFLICT DO NOTHING + RETURNING id: returns the id on insert,
        // empty result on conflict. Only publish on actual inserts.
        let embed_bytes = crate::embeddings::vec_to_bytes(&new_vec);
        let mut rows = self
            .conn()
            .query(
                "INSERT INTO semantic_memories
                 (run_id, agent_id, scope, summary, details_json, embedding, created_at, idempotency_key)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(idempotency_key) DO NOTHING
                 RETURNING id",
                (
                    run_id,
                    agent_id,
                    scope,
                    summary,
                    details_str,
                    embed_bytes,
                    Utc::now().to_rfc3339(),
                    key.clone(),
                ),
            )
            .await
            .context("inserting semantic memory")?;
        if let Some(row) = rows.next().await? {
            let id: i64 = row.get(0)?;
            self.bus.publish(MemoryEvent::SemanticSaved {
                id,
                run_id: run_id.to_string(),
                scope: scope.to_string(),
                summary: summary.to_string(),
            });
            // Update the inverted index. We do this after the publish
            // so observers see the row before the index reflects it.
            self.reindex_semantic(id, summary, details.as_ref()).await?;
            return Ok(id);
        }
        // Conflict: look up the existing id and return it without publishing.
        let mut rows = self
            .conn()
            .query(
                "SELECT id FROM semantic_memories WHERE idempotency_key = ?1",
                [key],
            )
            .await
            .context("looking up semantic by idempotency key")?;
        if let Some(row) = rows.next().await? {
            Ok(row.get(0)?)
        } else {
            Err(MemoryError::Other(
                "semantic insert returned no row but row was not found".into(),
            ))
        }
    }

    /// Find an existing semantic memory with cosine similarity >= `threshold`
    /// against `query_vec`. Returns the id of the first match, or `None`.
    async fn find_near_duplicate(
        &self,
        query_vec: &[f32],
        threshold: f32,
    ) -> Result<Option<i64>, MemoryError> {
        let mut rows = self
            .conn()
            .query(
                "SELECT id, embedding FROM semantic_memories
                 WHERE archived = 0 AND embedding IS NOT NULL",
                (),
            )
            .await
            .context("querying embeddings for dedup")?;
        while let Some(row) = rows.next().await? {
            let id: i64 = row.get(0)?;
            let embed_bytes: Vec<u8> = row.get(1)?;
            let existing_vec = crate::embeddings::bytes_to_vec(&embed_bytes);
            if existing_vec.len() == query_vec.len() {
                let sim = crate::embeddings::cosine_similarity(query_vec, &existing_vec);
                if sim >= threshold {
                    return Ok(Some(id));
                }
            }
        }
        Ok(None)
    }

    /// Inverted-index search over `semantic_memories` for `query`.
    /// Returns full [`SemanticMemory`] rows, ranked by a simple
    /// BM25-like score (term frequency × inverse document frequency).
    /// When `query` is empty, returns the most recent rows.
    pub async fn search_semantic(
        &self,
        query: &str,
        limit: usize,
        _unused_run_bias: bool, // reserved; kept for API stability
    ) -> Result<Vec<SemanticMemory>, MemoryError> {
        let q = query.trim();
        if q.is_empty() {
            let mut rows = self
                .conn()
                .query(
                    "SELECT id, run_id, agent_id, scope, summary, details_json, importance, archived, created_at
                     FROM semantic_memories
                     WHERE archived = 0
                     ORDER BY id DESC
                     LIMIT ?1",
                    [limit as i64],
                )
                .await
                .context("querying recent semantic memories")?;
            return self.collect_semantic_rows(&mut rows).await;
        }
        let terms = tokenize(q);
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        // Single query: match all terms at once via IN clause.
        // Terms are from our tokenizer (not user input), so no injection risk.
        let term_list = terms
            .iter()
            .map(|t| format!("'{t}'"))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT memory_id, COUNT(*) FROM term_index
             WHERE memory_kind = 'semantic' AND term IN ({term_list})
             GROUP BY memory_id"
        );
        let mut scores: std::collections::HashMap<i64, f64> = std::collections::HashMap::new();
        let mut rows = self
            .conn()
            .query(&sql, ())
            .await
            .context("counting term matches")?;
        while let Some(row) = rows.next().await? {
            let id: i64 = row.get(0)?;
            let count: i64 = row.get(1)?;
            scores.insert(id, count as f64);
        }
        // Pull a generous candidate set; rank in Rust; truncate.
        let mut ids: Vec<(i64, f64)> = scores.into_iter().collect();
        ids.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        ids.truncate(limit * 2);
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        // Inline the IDs into the SQL. They're i64, never user input,
        // so there's no injection risk. This dodges the `IntoParams`
        // trait gymnastics needed for a dynamic `IN (?, ?, ...)`.
        let id_list = ids
            .iter()
            .map(|(id, _)| id.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT id, run_id, agent_id, scope, summary, details_json, importance, archived, created_at
             FROM semantic_memories
             WHERE id IN ({id_list}) AND archived = 0"
        );
        let mut rows = self
            .conn()
            .query(&sql, ())
            .await
            .context("fetching candidate semantic memories")?;
        let mut results = self.collect_semantic_rows(&mut rows).await?;
        // Re-rank in the order of the scores we computed.
        let id_to_score: std::collections::HashMap<i64, f64> = ids.into_iter().collect();
        results.sort_by(|a, b| {
            let sa = id_to_score.get(&a.id).copied().unwrap_or(0.0);
            let sb = id_to_score.get(&b.id).copied().unwrap_or(0.0);
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(limit);
        Ok(results)
    }

    pub(super) async fn collect_semantic_rows(
        &self,
        rows: &mut Rows,
    ) -> Result<Vec<SemanticMemory>, MemoryError> {
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            let details_str: Option<String> = row.get(5)?;
            let details = details_str
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .context("parsing semantic details")?;
            out.push(SemanticMemory {
                id: row.get(0)?,
                run_id: row.get(1)?,
                agent_id: row.get(2)?,
                scope: row.get(3)?,
                summary: row.get(4)?,
                details,
                importance: row.get(6)?,
                archived: row.get::<i64>(7)? != 0,
                created_at: row.get(8)?,
            });
        }
        for m in &out {
            self.bump_last_accessed("semantic", m.id).await;
        }
        Ok(out)
    }

    pub(super) async fn peek_semantic(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<PeekedMemory>, MemoryError> {
        let q = query.trim();
        // Empty query → fall back to most-recent rows. This mirrors
        // `search_semantic` so the two paths stay symmetric; UI callers
        // that pass `""` to get the timeline view get sensible results
        // instead of an empty Vec.
        if q.is_empty() {
            let mut rows = self
                .db
                .conn()
                .query(
                    "SELECT id, scope, summary FROM semantic_memories
                     WHERE archived = 0
                     ORDER BY id DESC
                     LIMIT ?1",
                    [limit as i64],
                )
                .await?;
            let mut out: Vec<PeekedMemory> = Vec::new();
            while let Some(row) = rows.next().await? {
                let id: i64 = row.get(0)?;
                let scope: String = row.get(1)?;
                let summary: String = row.get(2)?;
                out.push(PeekedMemory {
                    id,
                    kind: PeekedKind::Semantic,
                    summary,
                    label: scope,
                    tokens_approx: 0,
                });
            }
            return Ok(out);
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
             WHERE memory_kind = 'semantic' AND term IN ({term_list})
               AND memory_id IN (SELECT id FROM semantic_memories WHERE archived = 0)
             GROUP BY memory_id"
        );
        let mut scores: std::collections::HashMap<i64, f64> = std::collections::HashMap::new();
        let mut rows = self.db.conn().query(&sql, ()).await?;
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
        // Inline the IDs into the SQL (see note in `search_semantic`).
        let id_list = ids
            .iter()
            .map(|(id, _)| id.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT id, scope, summary FROM semantic_memories
             WHERE id IN ({id_list}) AND archived = 0"
        );
        let mut rows = self.db.conn().query(&sql, ()).await?;
        let mut by_id: std::collections::HashMap<i64, (String, String)> =
            std::collections::HashMap::new();
        while let Some(row) = rows.next().await? {
            let id: i64 = row.get(0)?;
            let scope: String = row.get(1)?;
            let summary: String = row.get(2)?;
            by_id.insert(id, (scope, summary));
        }
        let mut out: Vec<PeekedMemory> = ids
            .iter()
            .filter_map(|(id, _score)| {
                by_id.get(id).map(|(scope, summary)| PeekedMemory {
                    id: *id,
                    kind: PeekedKind::Semantic,
                    summary: summary.clone(),
                    label: scope.clone(),
                    tokens_approx: 0,
                })
            })
            .collect();
        // Sort by score descending (ids is already score-sorted, but
        // filter_map may have dropped some, so re-sort explicitly).
        let id_to_score: std::collections::HashMap<i64, f64> = ids.into_iter().collect();
        out.sort_by(|a, b| {
            let sa = id_to_score.get(&a.id).copied().unwrap_or(0.0);
            let sb = id_to_score.get(&b.id).copied().unwrap_or(0.0);
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        });
        for p in &out {
            self.bump_last_accessed("semantic", p.id).await;
        }
        Ok(out)
    }

    pub(super) async fn get_semantic_observation(
        &self,
        id: i64,
    ) -> Result<Option<Observation>, MemoryError> {
        let mut rows = self
            .conn()
            .query(
                "SELECT id, run_id, agent_id, scope, summary, details_json, importance, archived, created_at
                 FROM semantic_memories WHERE id = ?1 AND archived = 0",
                [id],
            )
            .await
            .context("querying semantic by id")?;
        if let Some(row) = rows.next().await? {
            let details_str: Option<String> = row.get(5)?;
            let details = details_str
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .context("parsing details")?;
            Ok(Some(Observation::Semantic(SemanticMemory {
                id: row.get(0)?,
                run_id: row.get(1)?,
                agent_id: row.get(2)?,
                scope: row.get(3)?,
                summary: row.get(4)?,
                details,
                importance: row.get(6)?,
                archived: row.get::<i64>(7)? != 0,
                created_at: row.get(8)?,
            })))
        } else {
            Ok(None)
        }
    }

    /// Replace the `term_index` rows for one semantic memory with the
    /// terms of `summary` + `details_json`.
    pub(super) async fn reindex_semantic(
        &self,
        id: i64,
        summary: &str,
        details: Option<&Value>,
    ) -> Result<(), MemoryError> {
        self.db
            .conn()
            .execute(
                "DELETE FROM term_index WHERE memory_kind = 'semantic' AND memory_id = ?1",
                [id],
            )
            .await
            .context("clearing semantic term_index")?;
        let details_text = details.map(|v| v.to_string()).unwrap_or_default();
        let terms = tokenize(&format!("{summary} {details_text}"));
        for term in terms {
            self.db
                .conn()
                .execute(
                    "INSERT OR IGNORE INTO term_index (memory_kind, memory_id, term) VALUES ('semantic', ?1, ?2)",
                    (id, term),
                )
                .await
                .context("indexing semantic term")?;
        }
        Ok(())
    }

    /// Count non-archived semantic memories missing an embedding.
    pub async fn count_unembedded(&self) -> Result<i64, MemoryError> {
        let mut rows = self
            .conn()
            .query(
                "SELECT COUNT(*) FROM semantic_memories WHERE archived = 0 AND embedding IS NULL",
                (),
            )
            .await
            .context("counting unembedded memories")?;
        if let Some(row) = rows.next().await? {
            Ok(row.get(0)?)
        } else {
            Ok(0)
        }
    }

    /// Fetch non-archived semantic memories missing an embedding, up to `limit`.
    pub async fn get_unembedded(
        &self,
        limit: usize,
    ) -> Result<Vec<SemanticMemory>, MemoryError> {
        let mut rows = self
            .conn()
            .query(
                "SELECT id, run_id, agent_id, scope, summary, details_json, importance, archived, created_at
                 FROM semantic_memories
                 WHERE archived = 0 AND embedding IS NULL
                 ORDER BY id DESC
                 LIMIT ?1",
                [limit as i64],
            )
            .await
            .context("querying unembedded memories")?;
        self.collect_semantic_rows(&mut rows).await
    }

    /// Update the embedding blob for a semantic memory.
    pub async fn update_embedding(&self, id: i64, embedding: &[u8]) -> Result<(), MemoryError> {
        self.db
            .conn()
            .execute(
                "UPDATE semantic_memories SET embedding = ?1 WHERE id = ?2",
                (embedding, id),
            )
            .await
            .context("updating embedding")?;
        Ok(())
    }

    /// Fetch all non-archived semantic memories that have embeddings.
    /// Used by the consolidation service for clustering.
    pub async fn get_embedded(
        &self,
        limit: usize,
    ) -> Result<Vec<(i64, Vec<f32>, f64)>, MemoryError> {
        let mut rows = self
            .conn()
            .query(
                "SELECT id, embedding, importance FROM semantic_memories
                 WHERE archived = 0 AND embedding IS NOT NULL
                 ORDER BY id DESC
                 LIMIT ?1",
                [limit as i64],
            )
            .await
            .context("querying embedded memories")?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            let id: i64 = row.get(0)?;
            let embed_bytes: Vec<u8> = row.get(1)?;
            let importance: f64 = row.get(2)?;
            let vec = crate::embeddings::bytes_to_vec(&embed_bytes);
            out.push((id, vec, importance));
        }
        Ok(out)
    }

    /// Archive a semantic memory (set archived = 1).
    pub async fn archive_semantic(&self, id: i64) -> Result<(), MemoryError> {
        self.db
            .conn()
            .execute(
                "UPDATE semantic_memories SET archived = 1 WHERE id = ?1",
                [id],
            )
            .await
            .context("archiving semantic memory")?;
        Ok(())
    }

    /// Count total non-archived semantic memories.
    pub async fn count_semantic(&self) -> Result<i64, MemoryError> {
        let mut rows = self
            .conn()
            .query(
                "SELECT COUNT(*) FROM semantic_memories WHERE archived = 0",
                (),
            )
            .await
            .context("counting semantic memories")?;
        if let Some(row) = rows.next().await? {
            Ok(row.get(0)?)
        } else {
            Ok(0)
        }
    }

    /// Look up `(run_id, created_at)` for a semantic observation. Used by
    /// [`super::super::context::get_timeline`].
    pub(super) async fn semantic_timeline_key(
        &self,
        observation_id: i64,
    ) -> Result<Option<(String, String)>, MemoryError> {
        let mut rows = self
            .conn()
            .query(
                "SELECT run_id, created_at FROM semantic_memories WHERE id = ?1",
                [observation_id],
            )
            .await?;
        if let Some(row) = rows.next().await? {
            Ok(Some((row.get(0)?, row.get(1)?)))
        } else {
            Ok(None)
        }
    }

    /// Suppress unused-import warning when `ObservationKind` is referenced
    /// only through `impl` return types in trait-adjacent contexts.
    #[allow(dead_code)]
    fn _kind_semantic() -> ObservationKind {
        ObservationKind::Semantic
    }
}