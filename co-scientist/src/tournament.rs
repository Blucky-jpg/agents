/// Tournament match tracking.
///
/// Stores pairwise comparisons between hypotheses and their rationales.
/// Used by the ranking agent and the meta-review agent.

use std::sync::Arc;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::db::Db;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TournamentMatch {
    pub id: i64,
    pub session_id: String,
    pub hypothesis_a: i64,
    pub hypothesis_b: i64,
    pub winner: i64, // 1 = A, 2 = B, 0 = draw
    pub rationale: Option<String>,
    pub created_at: String,
}

#[derive(Clone)]
pub struct TournamentRepo {
    db: Arc<Db>,
}

impl TournamentRepo {
    pub fn new(db: Arc<Db>) -> Self {
        Self { db }
    }

    /// Insert a tournament match. Returns the new id.
    pub async fn insert(
        &self,
        session_id: &str,
        hypothesis_a: i64,
        hypothesis_b: i64,
        winner: i64,
        rationale: Option<&str>,
    ) -> Result<i64> {
        let mut rows = self
            .db
            .conn()
            .query(
                "INSERT INTO tournament_matches (session_id, hypothesis_a, hypothesis_b, winner, rationale, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 RETURNING id",
                (
                    session_id,
                    hypothesis_a,
                    hypothesis_b,
                    winner,
                    rationale,
                    chrono::Utc::now().to_rfc3339(),
                ),
            )
            .await
            .context("inserting tournament match")?;
        let row = rows
            .next()
            .await?
            .ok_or_else(|| anyhow::anyhow!("no row returned from match insert"))?;
        Ok(row.get(0)?)
    }

    /// Total match count for a session.
    pub async fn match_count(&self, session_id: &str) -> Result<i64> {
        let mut rows = self
            .db
            .conn()
            .query(
                "SELECT COUNT(*) FROM tournament_matches WHERE session_id = ?1",
                [session_id],
            )
            .await?;
        if let Some(row) = rows.next().await? {
            Ok(row.get(0)?)
        } else {
            Ok(0)
        }
    }

    /// Recent rationales for meta-review context.
    pub async fn recent_rationales(&self, session_id: &str, limit: usize) -> Result<Vec<String>> {
        let mut rows = self
            .db
            .conn()
            .query(
                "SELECT rationale FROM tournament_matches
                 WHERE session_id = ?1 AND rationale IS NOT NULL
                 ORDER BY id DESC LIMIT ?2",
                (session_id, limit as i64),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            if let Some(r) = row.get::<Option<String>>(0)? {
                out.push(r);
            }
        }
        Ok(out)
    }

    /// Check if a specific pair has already been matched.
    pub async fn pair_already_matched(
        &self,
        session_id: &str,
        hyp_a: i64,
        hyp_b: i64,
    ) -> Result<bool> {
        let (lo, hi) = if hyp_a < hyp_b {
            (hyp_a, hyp_b)
        } else {
            (hyp_b, hyp_a)
        };
        let mut rows = self
            .db
            .conn()
            .query(
                "SELECT COUNT(*) FROM tournament_matches
                 WHERE session_id = ?1
                   AND ((hypothesis_a = ?2 AND hypothesis_b = ?3)
                     OR (hypothesis_a = ?3 AND hypothesis_b = ?2))",
                (session_id, lo, hi),
            )
            .await?;
        if let Some(row) = rows.next().await? {
            let count: i64 = row.get(0)?;
            Ok(count > 0)
        } else {
            Ok(false)
        }
    }
}
