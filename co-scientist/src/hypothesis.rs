/// Hypothesis model and DB operations.
///
/// Hypotheses are the core unit of the research pipeline. Each has a
/// state machine (draft → reviewed → in_tournament → ranked), an Elo
/// rating for tournament ranking, and a FK to `semantic_memories` for
/// the full text.

use std::sync::Arc;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::db::Db;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HypothesisState {
    Draft,
    Reviewed,
    InTournament,
    Ranked,
    Pinned,
    Rejected,
}

impl HypothesisState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Reviewed => "reviewed",
            Self::InTournament => "in_tournament",
            Self::Ranked => "ranked",
            Self::Pinned => "pinned",
            Self::Rejected => "rejected",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "draft" => Some(Self::Draft),
            "reviewed" => Some(Self::Reviewed),
            "in_tournament" => Some(Self::InTournament),
            "ranked" => Some(Self::Ranked),
            "pinned" => Some(Self::Pinned),
            "rejected" => Some(Self::Rejected),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hypothesis {
    pub id: i64,
    pub session_id: String,
    pub state: HypothesisState,
    pub elo: f64,
    pub parent_ids: Vec<i64>,
    pub semantic_id: Option<i64>,
    pub matches_played: i64,
    pub created_at: String,
}

#[derive(Clone)]
pub struct HypothesisRepo {
    db: Arc<Db>,
}

impl HypothesisRepo {
    pub fn new(db: Arc<Db>) -> Self {
        Self { db }
    }

    /// Insert a new hypothesis. Returns the new id.
    pub async fn insert(
        &self,
        session_id: &str,
        semantic_id: Option<i64>,
        parent_ids: &[i64],
        initial_elo: f64,
    ) -> Result<i64> {
        let parent_json = if parent_ids.is_empty() {
            None
        } else {
            Some(serde_json::to_string(parent_ids).unwrap_or_default())
        };
        let mut rows = self
            .db
            .conn()
            .query(
                "INSERT INTO hypotheses (session_id, state, elo, parent_ids, semantic_id, matches_played, created_at)
                 VALUES (?1, 'draft', ?2, ?3, ?4, 0, ?5)
                 RETURNING id",
                (
                    session_id,
                    initial_elo,
                    parent_json,
                    semantic_id,
                    chrono::Utc::now().to_rfc3339(),
                ),
            )
            .await
            .context("inserting hypothesis")?;
        let row = rows
            .next()
            .await?
            .ok_or_else(|| anyhow::anyhow!("no row returned from hypothesis insert"))?;
        Ok(row.get(0)?)
    }

    /// Get a hypothesis by id.
    pub async fn get(&self, id: i64) -> Result<Option<Hypothesis>> {
        let mut rows = self
            .db
            .conn()
            .query(
                "SELECT id, session_id, state, elo, parent_ids, semantic_id, matches_played, created_at
                 FROM hypotheses WHERE id = ?1",
                [id],
            )
            .await
            .context("getting hypothesis")?;
        if let Some(row) = rows.next().await? {
            Ok(Some(row_to_hypothesis(&row)?))
        } else {
            Ok(None)
        }
    }

    /// List all hypotheses for a session.
    pub async fn list_by_session(&self, session_id: &str) -> Result<Vec<Hypothesis>> {
        let mut rows = self
            .db
            .conn()
            .query(
                "SELECT id, session_id, state, elo, parent_ids, semantic_id, matches_played, created_at
                 FROM hypotheses WHERE session_id = ?1 ORDER BY elo DESC",
                [session_id],
            )
            .await
            .context("listing hypotheses")?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(row_to_hypothesis(&row)?);
        }
        Ok(out)
    }

    /// Update hypothesis state. Only transitions forward (won't clobber
    /// a more advanced state unless `force` is true).
    pub async fn update_state(&self, id: i64, state: HypothesisState, force: bool) -> Result<()> {
        if force {
            self.db
                .conn()
                .execute(
                    "UPDATE hypotheses SET state = ?1 WHERE id = ?2",
                    (state.as_str(), id),
                )
                .await?;
        } else {
            // Only advance state, don't regress.
            let _order = |s: &str| match s {
                "draft" => 0,
                "reviewed" => 1,
                "in_tournament" => 2,
                "ranked" => 3,
                "pinned" => 99,
                "rejected" => 99,
                _ => 0,
            };
            self.db
                .conn()
                .execute(
                    "UPDATE hypotheses SET state = ?1
                     WHERE id = ?2 AND (
                         CASE state
                             WHEN 'draft' THEN 0
                             WHEN 'reviewed' THEN 1
                             WHEN 'in_tournament' THEN 2
                             WHEN 'ranked' THEN 3
                             WHEN 'pinned' THEN 99
                             WHEN 'rejected' THEN 99
                             ELSE 0
                         END
                     ) < CASE ?1
                         WHEN 'draft' THEN 0
                         WHEN 'reviewed' THEN 1
                         WHEN 'in_tournament' THEN 2
                         WHEN 'ranked' THEN 3
                         WHEN 'pinned' THEN 99
                         WHEN 'rejected' THEN 99
                         ELSE 0
                     END",
                    (state.as_str(), id),
                )
                .await?;
        }
        Ok(())
    }

    /// Update Elo rating and match count.
    pub async fn update_elo(&self, id: i64, new_elo: f64, matches_played: i64) -> Result<()> {
        self.db
            .conn()
            .execute(
                "UPDATE hypotheses SET elo = ?1, matches_played = ?2 WHERE id = ?3",
                (new_elo, matches_played, id),
            )
            .await
            .context("updating hypothesis elo")?;
        Ok(())
    }

    /// Top N hypotheses by Elo for a session.
    pub async fn top_n(&self, session_id: &str, n: usize) -> Result<Vec<Hypothesis>> {
        let mut rows = self
            .db
            .conn()
            .query(
                "SELECT id, session_id, state, elo, parent_ids, semantic_id, matches_played, created_at
                 FROM hypotheses WHERE session_id = ?1 AND state NOT IN ('rejected', 'pinned')
                 ORDER BY elo DESC LIMIT ?2",
                (session_id, n as i64),
            )
            .await
            .context("listing top hypotheses")?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(row_to_hypothesis(&row)?);
        }
        Ok(out)
    }

    /// Count hypotheses with >= threshold matches played.
    pub async fn mature_count(&self, session_id: &str, threshold: i64) -> Result<i64> {
        let mut rows = self
            .db
            .conn()
            .query(
                "SELECT COUNT(*) FROM hypotheses
                 WHERE session_id = ?1 AND matches_played >= ?2 AND state NOT IN ('rejected', 'pinned')",
                (session_id, threshold),
            )
            .await?;
        if let Some(row) = rows.next().await? {
            Ok(row.get(0)?)
        } else {
            Ok(0)
        }
    }

    /// Count all non-rejected hypotheses for a session.
    pub async fn total_count(&self, session_id: &str) -> Result<i64> {
        let mut rows = self
            .db
            .conn()
            .query(
                "SELECT COUNT(*) FROM hypotheses
                 WHERE session_id = ?1 AND state NOT IN ('rejected', 'pinned')",
                [session_id],
            )
            .await?;
        if let Some(row) = rows.next().await? {
            Ok(row.get(0)?)
        } else {
            Ok(0)
        }
    }

    /// Get hypotheses that need tournament matches (fewer than threshold matches).
    pub async fn needs_matches(&self, session_id: &str, threshold: i64, limit: usize) -> Result<Vec<Hypothesis>> {
        let mut rows = self
            .db
            .conn()
            .query(
                "SELECT id, session_id, state, elo, parent_ids, semantic_id, matches_played, created_at
                 FROM hypotheses
                 WHERE session_id = ?1 AND state IN ('in_tournament', 'ranked') AND matches_played < ?2
                 ORDER BY matches_played ASC, elo DESC
                 LIMIT ?3",
                (session_id, threshold, limit as i64),
            )
            .await
            .context("listing hypotheses needing matches")?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(row_to_hypothesis(&row)?);
        }
        Ok(out)
    }
}

fn row_to_hypothesis(row: &crate::db::OwnedRow) -> Result<Hypothesis> {
    let state_str: String = row.get(2)?;
    let state = HypothesisState::parse(&state_str)
        .ok_or_else(|| anyhow::anyhow!("unknown hypothesis state: {state_str}"))?;
    let parent_json: Option<String> = row.get(4)?;
    let parent_ids: Vec<i64> = parent_json
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();
    Ok(Hypothesis {
        id: row.get(0)?,
        session_id: row.get(1)?,
        state,
        elo: row.get(3)?,
        parent_ids,
        semantic_id: row.get(5)?,
        matches_played: row.get(6)?,
        created_at: row.get(7)?,
    })
}
