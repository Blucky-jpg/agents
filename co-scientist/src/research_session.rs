//! `research_sessions` table persistence.
//!
//! The Supervisor orchestrator used to hand-write SQL for the
//! `research_sessions` table directly; this repo collects every
//! `research_sessions` query behind a single typed seam so the
//! orchestrator can stay focused on coordination.
//!
//! Mirrors the [`HypothesisRepo`](crate::hypothesis::HypothesisRepo)
//! pattern: cheap `Clone`, owns an `Arc<Db>`, every method is async
//! with `Context as _` error annotations.

use std::sync::Arc;

use anyhow::{Context as _, Result};

use crate::db::Db;

#[derive(Clone)]
pub struct ResearchSessionRepo {
    db: Arc<Db>,
}

impl ResearchSessionRepo {
    pub fn new(db: Arc<Db>) -> Self {
        Self { db }
    }

    /// At startup, mark any `running` session that is NOT the current
    /// `running_session_id` as `interrupted`. Zombies from prior crashed
    /// processes land here.
    pub async fn recover_stale(&self, running_session_id: &str) -> Result<usize> {
        self.db
            .conn()
            .execute(
                "UPDATE research_sessions
                    SET status = 'interrupted',
                        ended_at = COALESCE(ended_at, ?1)
                  WHERE status = 'running'
                    AND id != ?2",
                (chrono::Utc::now().to_rfc3339(), running_session_id),
            )
            .await
            .context("recovering stale running sessions")
    }

    /// At startup, cancel any task whose session is no longer `running`.
    /// Catches reflection tasks enqueued milliseconds before the prior
    /// process died.
    pub async fn cancel_orphaned_tasks(&self) -> Result<usize> {
        self.db
            .conn()
            .execute(
                "UPDATE tasks SET status = 'cancelled',
                        finished_at = ?1,
                        last_error = '[startup-recovery]'
                  WHERE status IN ('pending', 'leased')
                    AND session_id NOT IN (
                        SELECT id FROM research_sessions WHERE status = 'running'
                    )",
                (chrono::Utc::now().to_rfc3339(),),
            )
            .await
            .context("recovering stale tasks")
    }

    /// Persist a brand-new session row with status='running'.
    pub async fn create(
        &self,
        id: &str,
        goal: &str,
        preferences: &str,
        budget_usd: Option<f64>,
        started_at: &str,
    ) -> Result<()> {
        let _ = self
            .db
            .conn()
            .execute(
                "INSERT INTO research_sessions (id, goal, preferences, status, budget_usd, started_at)
                 VALUES (?1, ?2, ?3, 'running', ?4, ?5)",
                (id, goal, preferences, budget_usd, started_at),
            )
            .await
            .context("creating research session")?;
        Ok(())
    }

    /// Mark a session fully complete with the final report text.
    pub async fn mark_done_with_report(
        &self,
        session_id: &str,
        report: &str,
        ended_at: &str,
    ) -> Result<()> {
        let _ = self
            .db
            .conn()
            .execute(
                "UPDATE research_sessions SET final_report = ?1, status = 'done', ended_at = ?2 WHERE id = ?3",
                (report, ended_at, session_id),
            )
            .await?;
        Ok(())
    }

    /// Mark a session complete without writing a final report (used when
    /// the final-report agent itself failed).
    pub async fn finalize(&self, session_id: &str, ended_at: &str) -> Result<()> {
        let _ = self
            .db
            .conn()
            .execute(
                "UPDATE research_sessions SET status = 'done', ended_at = ?1 WHERE id = ?2",
                (ended_at, session_id),
            )
            .await?;
        Ok(())
    }
}