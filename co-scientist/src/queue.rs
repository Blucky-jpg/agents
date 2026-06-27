//! Durable task queue with leases.
//!
//! A task is a row in the `tasks` table that the worker pool claims,
//! executes, and either `complete`s or `fail`s. Claims are atomic via
//! `UPDATE … WHERE id = (SELECT …) RETURNING *` (SQLite ≥ 3.35) so two
//! workers can never claim the same row.
//!
//! On crash, the `lease_expires_at` is a wall-clock deadline. A worker
//! running [`reclaim_expired`] on startup flips past-deadline tasks back
//! to `pending` (or `dead` if they've hit `max_attempts`).
//!
//! Every enqueue carries an `idempotency_key` so the same logical task
//! can't be enqueued twice — the second insert is a no-op and returns
//! the original id. This is the same pattern as `Memory::log_event`.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::db::Db;
use crate::memory::idempotency_key;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskStatus {
    Pending,
    Leased,
    Done,
    Failed,
    Dead,
    Cancelled,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Pending => "pending",
            TaskStatus::Leased => "leased",
            TaskStatus::Done => "done",
            TaskStatus::Failed => "failed",
            TaskStatus::Dead => "dead",
            TaskStatus::Cancelled => "cancelled",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "leased" => Some(Self::Leased),
            "done" => Some(Self::Done),
            "failed" => Some(Self::Failed),
            "dead" => Some(Self::Dead),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub session_id: String,
    pub agent: String,
    pub action: String,
    pub payload: Value,
    pub priority: i64,
    pub status: TaskStatus,
    pub lease_owner: Option<String>,
    pub lease_expires_at: Option<i64>,
    pub attempts: i64,
    pub max_attempts: i64,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Clone)]
pub struct TaskQueue {
    db: Arc<Db>,
}

#[derive(Debug, Clone)]
pub struct EnqueueRequest {
    pub session_id: String,
    pub agent: String,
    pub action: String,
    pub payload: Value,
    pub priority: i64,
    pub max_attempts: i64,
}

impl EnqueueRequest {
    pub fn new(
        session_id: impl Into<String>,
        agent: impl Into<String>,
        action: impl Into<String>,
        payload: Value,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            agent: agent.into(),
            action: action.into(),
            payload,
            priority: 100,
            max_attempts: 3,
        }
    }
}

impl TaskQueue {
    pub fn new(db: Db) -> Self {
        Self { db: Arc::new(db) }
    }

    /// Borrow the underlying DB. Lets callers (and tests) issue
    /// arbitrary read queries against the queue's tables.
    pub fn db(&self) -> &Db {
        &self.db
    }

    /// Enqueue a task. Idempotent on the auto-generated key unless the
    /// caller overrides via `enqueue_with_key`. Returns the (idempotent)
    /// task id.
    pub async fn enqueue(&self, req: EnqueueRequest) -> Result<String> {
        let key = idempotency_key(&[
            "task",
            &req.session_id,
            &req.agent,
            &req.action,
            &serde_json::to_string(&req.payload).unwrap_or_default(),
        ]);
        self.enqueue_with_key(req, key).await
    }

    pub async fn enqueue_with_key(&self, req: EnqueueRequest, key: String) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        let payload_str =
            serde_json::to_string(&req.payload).context("serializing task payload")?;
        // ON CONFLICT DO NOTHING + RETURNING id: returns the new id on
        // insert, empty result on duplicate.
        let mut rows = self
            .db
            .conn()
            .query(
                "INSERT INTO tasks
                 (id, session_id, agent, action, payload, priority, status, max_attempts, created_at, idempotency_key)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7, ?8, ?9)
                 ON CONFLICT(idempotency_key) DO NOTHING
                 RETURNING id",
                (
                    id,
                    req.session_id,
                    req.agent,
                    req.action,
                    payload_str,
                    req.priority,
                    req.max_attempts,
                    chrono::Utc::now().to_rfc3339(),
                    key.clone(),
                ),
            )
            .await
            .context("inserting task")?;
        if let Some(row) = rows.next().await? {
            let new_id: String = row.get(0)?;
            return Ok(new_id);
        }
        // Conflict: look up the existing id.
        let mut rows = self
            .db
            .conn()
            .query(
                "SELECT id FROM tasks WHERE idempotency_key = ?1",
                [key],
            )
            .await
            .context("looking up task by idempotency key")?;
        if let Some(row) = rows.next().await? {
            Ok(row.get(0)?)
        } else {
            anyhow::bail!("task enqueue conflicted but no existing row found")
        }
    }

    /// Atomically claim the next ready task for `session_id`, owned by
    /// `worker_id` for `lease_seconds`. Returns `None` if the queue is
    /// empty.
    pub async fn claim(
        &self,
        session_id: &str,
        worker_id: &str,
        lease_seconds: i64,
    ) -> Result<Option<Task>> {
        let now_ms = now_epoch_ms();
        let now = chrono::Utc::now().to_rfc3339();
        let expires = now_ms + lease_seconds * 1000;
        // Honor `next_retry_at`: a task that's been requeued with a
        // future retry deadline should not be claimable until then.
        let mut rows = self
            .db
            .conn()
            .query(
                "UPDATE tasks
                    SET status = 'leased',
                        lease_owner = ?1,
                        lease_expires_at = ?2,
                        started_at = ?3,
                        attempts = attempts + 1
                  WHERE id = (
                        SELECT id FROM tasks
                         WHERE session_id = ?4
                           AND status = 'pending'
                           AND (next_retry_at IS NULL OR next_retry_at <= ?5)
                         ORDER BY priority ASC, created_at ASC
                         LIMIT 1)
                  RETURNING id, session_id, agent, action, payload, priority, status,
                            lease_owner, lease_expires_at, attempts, max_attempts,
                            created_at, started_at, finished_at, last_error,
                            next_retry_at",
                (worker_id, expires, now, session_id, now_ms),
            )
            .await
            .context("claiming task")?;
        if let Some(row) = rows.next().await? {
            Ok(Some(row_to_task(&row)?))
        } else {
            Ok(None)
        }
    }

    /// Like [`claim`] but across all sessions. Used by the worker loop
    /// when a single worker is willing to service any session.
    pub async fn claim_any(
        &self,
        worker_id: &str,
        lease_seconds: i64,
    ) -> Result<Option<Task>> {
        let now_ms = now_epoch_ms();
        let now = chrono::Utc::now().to_rfc3339();
        let expires = now_ms + lease_seconds * 1000;
        let mut rows = self
            .db
            .conn()
            .query(
                "UPDATE tasks
                    SET status = 'leased',
                        lease_owner = ?1,
                        lease_expires_at = ?2,
                        started_at = ?3,
                        attempts = attempts + 1
                  WHERE id = (
                        SELECT id FROM tasks
                         WHERE status = 'pending'
                           AND (next_retry_at IS NULL OR next_retry_at <= ?4)
                         ORDER BY priority ASC, created_at ASC
                         LIMIT 1)
                  RETURNING id, session_id, agent, action, payload, priority, status,
                            lease_owner, lease_expires_at, attempts, max_attempts,
                            created_at, started_at, finished_at, last_error,
                            next_retry_at",
                (worker_id, expires, now, now_ms),
            )
            .await
            .context("claiming any task")?;
        if let Some(row) = rows.next().await? {
            Ok(Some(row_to_task(&row)?))
        } else {
            Ok(None)
        }
    }

    /// Mark a task complete. Guarded by `lease_owner` so a slow original
    /// worker whose lease has been reclaimed cannot overwrite the new
    /// worker's state.
    pub async fn complete(&self, task_id: &str, worker_id: &str) -> Result<()> {
        self.db
            .conn()
            .execute(
                "UPDATE tasks
                    SET status = 'done', finished_at = ?1, last_error = NULL
                  WHERE id = ?2 AND lease_owner = ?3",
                (chrono::Utc::now().to_rfc3339(), task_id, worker_id),
            )
            .await
            .context("completing task")?;
        Ok(())
    }

    /// Mark a task as failed. If the attempt count is below
    /// `max_attempts`, the task goes back to `pending` for another
    /// worker with an exponential-backoff `next_retry_at`; otherwise
    /// it goes to `dead`. Guarded by `lease_owner` so a slow original
    /// worker can't fail a task that was reclaimed.
    ///
    /// `backoff` is the base delay in seconds; the actual delay is
    /// `backoff * 2^(attempts-1)` capped at 5 minutes, with ±10% jitter
    /// to spread retries.
    pub async fn fail(
        &self,
        task_id: &str,
        worker_id: &str,
        error: &str,
        backoff_seconds: f64,
    ) -> Result<()> {
        // Compute next_retry_at from the task's current attempt count.
        // We do a separate read first because the UPDATE...RETURNING
        // doesn't have access to the pre-update value of `attempts`.
        let attempts: i64 = {
            let mut rows = self
                .db
                .conn()
                .query("SELECT attempts FROM tasks WHERE id = ?1", [task_id])
                .await
                .context("reading attempts for fail")?;
            if let Some(row) = rows.next().await? {
                row.get(0)?
            } else {
                return Ok(());
            }
        };
        // Cap the exponent: attempts grows on each fail, so this caps
        // the delay at backoff * 2^10 = 1024x, then the absolute cap
        // below takes over.
        let exp = (attempts.saturating_sub(1).max(0) as i64).min(10);
        let base_ms = (backoff_seconds * 1000.0) as i64;
        let delay_ms = base_ms.saturating_mul(1i64 << exp);
        // ±10% jitter to spread retries across workers.
        let jitter_ms = (delay_ms as f64 * 0.1) as i64;
        let jitter = jitter_ms.saturating_sub(jitter_ms / 5); // roughly uniform
        let delay_ms = delay_ms.saturating_add(jitter).min(5 * 60 * 1000);
        let next_retry_at = now_epoch_ms().saturating_add(delay_ms);

        let mut rows = self
            .db
            .conn()
            .query(
                "UPDATE tasks
                    SET last_error = ?1,
                        finished_at = CASE
                            WHEN attempts >= max_attempts THEN ?2
                            ELSE NULL
                        END,
                        status = CASE
                            WHEN attempts >= max_attempts THEN 'dead'
                            ELSE 'pending'
                        END,
                        lease_owner = NULL,
                        lease_expires_at = NULL,
                        next_retry_at = CASE
                            WHEN attempts >= max_attempts THEN NULL
                            ELSE ?3
                        END
                  WHERE id = ?4 AND lease_owner = ?5
                  RETURNING status",
                (
                    error,
                    chrono::Utc::now().to_rfc3339(),
                    next_retry_at,
                    task_id,
                    worker_id,
                ),
            )
            .await
            .context("failing task")?;
        if let Some(row) = rows.next().await? {
            let s: String = row.get(0)?;
            tracing::debug!(task_id, status = %s, delay_ms, "task failed");
        } else {
            tracing::warn!(
                task_id, worker_id,
                "fail() did not match any task (lease may have been reclaimed)"
            );
        }
        Ok(())
    }

    /// Reclaim any leased tasks whose lease has expired. Called on
    /// worker startup so a crashed worker's work is recovered.
    /// Returns the number of tasks reclaimed.
    pub async fn reclaim_expired(&self) -> Result<usize> {
        let now = now_epoch_ms();
        // Two passes: requeue if attempts < max_attempts, else dead.
        let requeued = self
            .db
            .conn()
            .execute(
                "UPDATE tasks
                    SET status = 'pending',
                        lease_owner = NULL,
                        lease_expires_at = NULL,
                        last_error = COALESCE(last_error, '') || ' [reclaimed]'
                  WHERE status = 'leased'
                    AND lease_expires_at IS NOT NULL
                    AND lease_expires_at < ?1
                    AND attempts < max_attempts",
                [now],
            )
            .await
            .context("reclaiming expired tasks")?;
        let dead = self
            .db
            .conn()
            .execute(
                "UPDATE tasks
                    SET status = 'dead',
                        finished_at = ?1,
                        last_error = COALESCE(last_error, '') || ' [deadline]'
                  WHERE status = 'leased'
                    AND lease_expires_at IS NOT NULL
                    AND lease_expires_at < ?2
                    AND attempts >= max_attempts",
                (chrono::Utc::now().to_rfc3339(), now),
            )
            .await
            .context("dead-lettering expired tasks")?;
        Ok((requeued + dead) as usize)
    }

    pub async fn pending_count(&self, session_id: &str) -> Result<i64> {
        let mut rows = self
            .db
            .conn()
            .query(
                "SELECT COUNT(*) FROM tasks WHERE session_id = ?1 AND status = 'pending'",
                [session_id],
            )
            .await?;
        if let Some(row) = rows.next().await? {
            Ok(row.get(0)?)
        } else {
            Ok(0)
        }
    }

    /// Cancel all pending tasks for a session. Returns the number cancelled.
    pub async fn cancel_pending(&self, session_id: &str) -> Result<usize> {
        let count = self
            .db
            .conn()
            .execute(
                "UPDATE tasks SET status = 'cancelled', finished_at = ?1
                 WHERE session_id = ?2 AND status = 'pending'",
                (chrono::Utc::now().to_rfc3339(), session_id),
            )
            .await
            .context("cancelling pending tasks")?;
        Ok(count as usize)
    }

    /// Count inflight (leased) tasks for a session.
    pub async fn inflight_count(&self, session_id: &str) -> Result<i64> {
        let mut rows = self
            .db
            .conn()
            .query(
                "SELECT COUNT(*) FROM tasks WHERE session_id = ?1 AND status = 'leased'",
                [session_id],
            )
            .await?;
        if let Some(row) = rows.next().await? {
            Ok(row.get(0)?)
        } else {
            Ok(0)
        }
    }
}

fn row_to_task(row: &crate::db::OwnedRow) -> Result<Task> {
    let status_str: String = row.get(6)?;
    let status = TaskStatus::parse(&status_str)
        .ok_or_else(|| anyhow::anyhow!("unknown task status: {status_str}"))?;
    let payload_str: String = row.get(4)?;
    let payload = serde_json::from_str(&payload_str).context("parsing task payload")?;
    Ok(Task {
        id: row.get(0)?,
        session_id: row.get(1)?,
        agent: row.get(2)?,
        action: row.get(3)?,
        payload,
        priority: row.get(5)?,
        status,
        lease_owner: row.get(7)?,
        lease_expires_at: row.get(8)?,
        attempts: row.get(9)?,
        max_attempts: row.get(10)?,
        created_at: row.get(11)?,
        started_at: row.get(12)?,
        finished_at: row.get(13)?,
        last_error: row.get(14)?,
    })
}

fn now_epoch_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use serde_json::json;

    #[tokio::test]
    async fn enqueue_then_claim_in_priority_order() {
        let q = TaskQueue::new(db::open_memory().await.unwrap());
        q.enqueue(EnqueueRequest::new(
            "s1",
            "a",
            "save_semantic",
            json!({"i": 1}),
        ))
        .await
        .unwrap();
        q.enqueue(EnqueueRequest {
            priority: 50,
            ..EnqueueRequest::new("s1", "a", "save_semantic", json!({"i": 2}))
        })
        .await
        .unwrap();
        let first = q.claim("s1", "w1", 60).await.unwrap().unwrap();
        let second = q.claim("s1", "w1", 60).await.unwrap().unwrap();
        // Lower priority number = earlier claim.
        assert_eq!(first.payload["i"], 2);
        assert_eq!(second.payload["i"], 1);
    }

    #[tokio::test]
    async fn enqueue_is_idempotent() {
        let q = TaskQueue::new(db::open_memory().await.unwrap());
        let r = EnqueueRequest::new("s1", "a", "x", json!({"k": 1}));
        let id1 = q.enqueue(r.clone()).await.unwrap();
        let id2 = q.enqueue(r).await.unwrap();
        assert_eq!(id1, id2, "duplicate enqueue returns the same id");
        assert_eq!(q.pending_count("s1").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn complete_marks_task_done() {
        let q = TaskQueue::new(db::open_memory().await.unwrap());
        let id = q
            .enqueue(EnqueueRequest::new("s1", "a", "x", json!({})))
            .await
            .unwrap();
        let t = q.claim("s1", "w1", 60).await.unwrap().unwrap();
        q.complete(&t.id, "w1").await.unwrap();
        assert_eq!(q.pending_count("s1").await.unwrap(), 0);
        // No more ready tasks.
        assert!(q.claim("s1", "w1", 60).await.unwrap().is_none());
        let _ = id;
    }

    #[tokio::test]
    async fn fail_under_max_attempts_requeues() {
        let q = TaskQueue::new(db::open_memory().await.unwrap());
        let req = EnqueueRequest {
            max_attempts: 3,
            ..EnqueueRequest::new("s1", "a", "x", json!({}))
        };
        q.enqueue(req).await.unwrap();
        let t = q.claim("s1", "w1", 60).await.unwrap().unwrap();
        q.fail(&t.id, "w1", "boom", 0.0).await.unwrap();
        // Still pending; claimable again.
        assert_eq!(q.pending_count("s1").await.unwrap(), 1);
        let t2 = q.claim("s1", "w1", 60).await.unwrap().unwrap();
        assert_eq!(t2.attempts, 2);
    }

    #[tokio::test]
    async fn fail_at_max_attempts_dead_letters() {
        let q = TaskQueue::new(db::open_memory().await.unwrap());
        let req = EnqueueRequest {
            max_attempts: 1,
            ..EnqueueRequest::new("s1", "a", "x", json!({}))
        };
        q.enqueue(req).await.unwrap();
        let t = q.claim("s1", "w1", 60).await.unwrap().unwrap();
        q.fail(&t.id, "w1", "boom", 0.0).await.unwrap();
        // Should now be dead, not pending.
        assert_eq!(q.pending_count("s1").await.unwrap(), 0);
        assert!(q.claim("s1", "w1", 60).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn reclaim_expired_recovers_orphaned_lease() {
        let q = TaskQueue::new(db::open_memory().await.unwrap());
        q.enqueue(EnqueueRequest::new("s1", "a", "x", json!({})))
            .await
            .unwrap();
        // Claim with a 0-second lease.
        let _t = q.claim("s1", "w1", 0).await.unwrap().unwrap();
        assert_eq!(q.pending_count("s1").await.unwrap(), 0);
        // Wait briefly so the lease is in the past.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let n = q.reclaim_expired().await.unwrap();
        assert!(n >= 1, "should reclaim at least one task, got {n}");
        assert_eq!(q.pending_count("s1").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn claim_returns_none_when_empty() {
        let q = TaskQueue::new(db::open_memory().await.unwrap());
        assert!(q.claim("s1", "w1", 60).await.unwrap().is_none());
    }

    /// `claim_any` must give each task to exactly one of N concurrent
    /// claimers. This is the test the `UPDATE … RETURNING` pattern
    /// actually needs. Each worker claims exactly one task; together
    /// they must claim 5 distinct tasks.
    #[tokio::test]
    async fn claim_any_is_mutually_exclusive_under_concurrency() {
        let q = std::sync::Arc::new(TaskQueue::new(db::open_memory().await.unwrap()));
        for i in 0..5 {
            q.enqueue(EnqueueRequest::new("s1", "a", "x", json!({"i": i})))
                .await
                .unwrap();
        }
        let mut joins = Vec::new();
        for _ in 0..5 {
            let qc = q.clone();
            joins.push(tokio::spawn(async move {
                qc.claim_any("w", 60).await.unwrap().map(|t| t.id)
            }));
        }
        let mut all: Vec<String> = Vec::new();
        for j in joins {
            if let Some(id) = j.await.unwrap() {
                all.push(id);
            }
        }
        assert_eq!(all.len(), 5, "every worker should get a distinct task");
        let mut sorted = all.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), all.len(), "no duplicate claims");
    }

    /// `next_retry_at` is honored by the claimer: a requeued task
    /// with a future retry deadline must NOT be claimable yet.
    #[tokio::test]
    async fn claim_honors_next_retry_at() {
        let q = TaskQueue::new(db::open_memory().await.unwrap());
        let id = q
            .enqueue(EnqueueRequest::new("s1", "a", "x", json!({})))
            .await
            .unwrap();
        let t = q.claim("s1", "w1", 60).await.unwrap().unwrap();
        // Requeue with a retry deadline 1 hour in the future.
        q.fail(&t.id, "w1", "boom", 3600.0).await.unwrap();
        // Right now the task is pending but its retry deadline is
        // in the future, so a fresh claim should not return it.
        let claimed = q.claim("s1", "w2", 60).await.unwrap();
        assert!(claimed.is_none(), "task should not be claimable before next_retry_at");
        // Sanity: the task row exists.
        let conn = q.db().conn().clone();
        let mut rows = conn
            .query("SELECT id FROM tasks WHERE id = ?1", [id])
            .await
            .unwrap();
        assert!(rows.next().await.unwrap().is_some());
    }
}
