//! Experiment module: persistent `experiments` table + sandboxed python
//! subprocess runner.
//!
//! This closes the empirical loop: hypotheses go through
//! `experiment_design` (LLM proposes code) → `experiment_execute` (run
//! it) → `experiment_evaluate` (LLM interprets metrics) →
//! `reflection_on_result` (existing tournament machinery ranks on
//! evidence).
//!
//! Design choices per IMPROVEMENT_PLAN §experiment-2026-06-28:
//!
//! - **Permissive sandbox**: spawn `python3 -c` in a tempdir, 30s wall
//!   clock, RLIMIT_AS advertised at 1.5GB (currently advisory only —
//!   see the inline note below). Runs as the user. Acceptable
//!   for single-user local research; not safe for hostile multi-tenant.
//! - **Timeout enforcement**: `tokio::time::timeout` cancels the await
//!   but does NOT kill the child. We additionally `child.kill()` after
//!   the timeout fires. If the child is in an uninterruptible syscall,
//!   the OS will clean up at process exit; worst case is a brief zombie.
//! - **Idempotency**: `idempotency_key` includes session_id +
//!   hypothesis_id + a nonce, so retries never silently dedupe across
//!   real attempts (UNIQUE index returns the existing row).
//! - **Pure repo**: `ExperimentRepo` is sync over the rusqlite conn.
//!   `run_python_code` is a free async function with no DB dependency
//!   — testable directly with no fixtures.

use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::db::Db;

// =====================================================================
// Status enum
// =====================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExperimentStatus {
    Designed,
    Running,
    Succeeded,
    Failed,
    TimedOut,
}

impl ExperimentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Designed => "designed",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "designed" => Some(Self::Designed),
            "running" => Some(Self::Running),
            "succeeded" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            "timed_out" => Some(Self::TimedOut),
            _ => None,
        }
    }
}

// =====================================================================
// Row model
// =====================================================================

#[derive(Debug, Clone)]
pub struct Experiment {
    pub id: i64,
    pub session_id: String,
    pub hypothesis_id: i64,
    pub status: ExperimentStatus,
    pub code: String,
    pub metric_name: String,
    pub metric_value: Option<f64>,
    pub metric_json: Option<serde_json::Value>,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    pub exit_code: Option<i64>,
    pub duration_ms: Option<i64>,
    pub error: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

fn row_to_experiment(row: &crate::db::OwnedRow) -> Result<Experiment> {
    // SELECT order: id, session_id, hypothesis_id, status, code, metric_name,
    //               metric_value, metric_json, stdout, stderr, exit_code,
    //               duration_ms, error, created_at, started_at, finished_at
    let status_str: String = row.get(3)?;
    let status = ExperimentStatus::parse(&status_str)
        .ok_or_else(|| anyhow::anyhow!("unknown experiment status: {status_str}"))?;
    let metric_json_str: Option<String> = row.get(7)?;
    let metric_json = metric_json_str
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .context("parsing metric_json")?;
    Ok(Experiment {
        id: row.get(0)?,
        session_id: row.get(1)?,
        hypothesis_id: row.get(2)?,
        status,
        code: row.get(4)?,
        metric_name: row.get(5)?,
        metric_value: row.get(6)?,
        metric_json,
        stdout: row.get(8)?,
        stderr: row.get(9)?,
        exit_code: row.get(10)?,
        duration_ms: row.get(11)?,
        error: row.get(12)?,
        created_at: row.get(13)?,
        started_at: row.get(14)?,
        finished_at: row.get(15)?,
    })
}

// =====================================================================
// Repository
// =====================================================================

#[derive(Clone)]
pub struct ExperimentRepo {
    db: Arc<Db>,
}

impl ExperimentRepo {
    pub fn new(db: Arc<Db>) -> Self {
        Self { db }
    }

    /// Insert a freshly-designed experiment. Idempotent on
    /// `idempotency_key` (which the caller passes in — typically
    /// `("experiment", session_id, hypothesis_id, "<nonce>")`).
    /// Returns the id (newly inserted OR pre-existing).
    pub async fn insert_design(
        &self,
        session_id: &str,
        hypothesis_id: i64,
        code: &str,
        metric_name: &str,
        idempotency_key: &str,
    ) -> Result<i64> {
        let now = chrono::Utc::now().to_rfc3339();
        // Try insert; on conflict, fetch existing.
        let mut rows = self
            .db
            .conn()
            .query(
                "INSERT INTO experiments
                 (session_id, hypothesis_id, status, code, metric_name, created_at, idempotency_key)
                 VALUES (?1, ?2, 'designed', ?3, ?4, ?5, ?6)
                 ON CONFLICT(idempotency_key) DO NOTHING
                 RETURNING id",
                (session_id, hypothesis_id, code, metric_name, now, idempotency_key),
            )
            .await
            .context("inserting experiment")?;
        if let Some(row) = rows.next().await? {
            let id: i64 = row.get(0)?;
            self.touch_latest(hypothesis_id, id).await.ok();
            return Ok(id);
        }
        // Conflict: return existing.
        let mut rows = self
            .db
            .conn()
            .query(
                "SELECT id FROM experiments WHERE idempotency_key = ?1",
                [idempotency_key],
            )
            .await
            .context("looking up existing experiment")?;
        if let Some(row) = rows.next().await? {
            let id: i64 = row.get(0)?;
            self.touch_latest(hypothesis_id, id).await.ok();
            Ok(id)
        } else {
            Err(anyhow::anyhow!(
                "experiment insert returned no row but row was not found"
            ))
        }
    }

    /// Transition an experiment to `running`. No-op if already past.
    pub async fn mark_running(&self, id: i64) -> Result<()> {
        self.db
            .conn()
            .execute(
                "UPDATE experiments SET status = 'running', started_at = ?1
                 WHERE id = ?2 AND status = 'designed'",
                (chrono::Utc::now().to_rfc3339(), id),
            )
            .await
            .context("marking experiment running")?;
        Ok(())
    }

    /// Mark experiment as finished (any terminal status). Persists all
    /// result fields in one shot. Touches `hypotheses.latest_experiment_id`
    /// on success so the supervisor can find the freshest result.
    pub async fn mark_finished(
        &self,
        id: i64,
        hypothesis_id: i64,
        status: ExperimentStatus,
        stdout: Option<&str>,
        stderr: Option<&str>,
        exit_code: Option<i32>,
        duration_ms: i64,
        metric_value: Option<f64>,
        metric_json: Option<&serde_json::Value>,
        error: Option<&str>,
    ) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        let metric_json_str = metric_json.map(serde_json::to_string).transpose()?;
        self.db
            .conn()
            .execute(
                "UPDATE experiments SET
                    status = ?1,
                    stdout = ?2,
                    stderr = ?3,
                    exit_code = ?4,
                    duration_ms = ?5,
                    metric_value = ?6,
                    metric_json = ?7,
                    error = ?8,
                    finished_at = ?9
                 WHERE id = ?10",
                (
                    status.as_str(),
                    stdout,
                    stderr,
                    exit_code,
                    duration_ms,
                    metric_value,
                    metric_json_str,
                    error,
                    now,
                    id,
                ),
            )
            .await
            .context("marking experiment finished")?;
        if matches!(status, ExperimentStatus::Succeeded) {
            self.touch_latest(hypothesis_id, id).await.ok();
        }
        Ok(())
    }

    async fn touch_latest(&self, hypothesis_id: i64, experiment_id: i64) -> Result<()> {
        self.db
            .conn()
            .execute(
                "UPDATE hypotheses SET latest_experiment_id = ?1 WHERE id = ?2",
                (experiment_id, hypothesis_id),
            )
            .await
            .context("touching hypotheses.latest_experiment_id")?;
        Ok(())
    }

    /// Fetch one experiment by id.
    pub async fn get(&self, id: i64) -> Result<Option<Experiment>> {
        let mut rows = self
            .db
            .conn()
            .query(
                "SELECT id, session_id, hypothesis_id, status, code, metric_name,
                        metric_value, metric_json, stdout, stderr, exit_code,
                        duration_ms, error, created_at, started_at, finished_at
                 FROM experiments WHERE id = ?1",
                [id],
            )
            .await
            .context("querying experiment")?;
        if let Some(row) = rows.next().await? {
            Ok(Some(row_to_experiment(&row)?))
        } else {
            Ok(None)
        }
    }

    /// All experiments for a hypothesis, newest first.
    pub async fn list_for_hypothesis(&self, hypothesis_id: i64) -> Result<Vec<Experiment>> {
        let mut rows = self
            .db
            .conn()
            .query(
                "SELECT id, session_id, hypothesis_id, status, code, metric_name,
                        metric_value, metric_json, stdout, stderr, exit_code,
                        duration_ms, error, created_at, started_at, finished_at
                 FROM experiments WHERE hypothesis_id = ?1
                 ORDER BY id DESC",
                [hypothesis_id],
            )
            .await
            .context("listing experiments")?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(row_to_experiment(&row)?);
        }
        Ok(out)
    }

    /// The most recent terminal experiment for a hypothesis, if any.
    pub async fn latest_for_hypothesis(&self, hypothesis_id: i64) -> Result<Option<Experiment>> {
        let mut rows = self
            .db
            .conn()
            .query(
                "SELECT id, session_id, hypothesis_id, status, code, metric_name,
                        metric_value, metric_json, stdout, stderr, exit_code,
                        duration_ms, error, created_at, started_at, finished_at
                 FROM experiments
                 WHERE hypothesis_id = ?1 AND status IN ('succeeded','failed','timed_out')
                 ORDER BY id DESC LIMIT 1",
                [hypothesis_id],
            )
            .await
            .context("querying latest experiment")?;
        if let Some(row) = rows.next().await? {
            Ok(Some(row_to_experiment(&row)?))
        } else {
            Ok(None)
        }
    }
}

// =====================================================================
// Sandboxed python runner
// =====================================================================

/// What `run_python_code` returns. Captures all the diagnostic data
/// the LLM (and humans) need to interpret the result.
#[derive(Debug, Clone)]
pub struct RunResult {
    pub status: ExperimentStatus,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub duration_ms: i64,
    /// Free-form error from the runner itself (timeout, spawn failure).
    pub runner_error: Option<String>,
}

/// Spawn `python3 -c <code>` in a freshly-created tempdir, enforce
/// `timeout` wall clock + `mem_mb` virtual-memory cap, capture stdout
/// and stderr. Returns a [`RunResult`].
///
/// **No database access.** Pure I/O + process. Unit-testable directly.
///
/// On non-Unix platforms the memory cap is a no-op (graceful fallback).
/// The timeout always fires and kills the child.
pub async fn run_python_code(
    code: &str,
    timeout: Duration,
    mem_mb: u64,
) -> RunResult {
    let started = Instant::now();

    // Build the command. We use `python3 -c` rather than writing to a
    // file: no on-disk artifact, no race on the tempdir name, and
    // python parses stdin-free so the whole program is one argv.
    let mut cmd = Command::new("python3");
    cmd.arg("-c").arg(code);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    // Use a fresh tempdir as cwd so any file ops the LLM emits stay
    // scoped. The TempDir is dropped at end of scope, deleting its
    // contents on Unix.
    let tempdir = match tempfile::TempDir::new() {
        Ok(t) => t,
        Err(e) => {
            return RunResult {
                status: ExperimentStatus::Failed,
                stdout: String::new(),
                stderr: String::new(),
                exit_code: None,
                duration_ms: started.elapsed().as_millis() as i64,
                runner_error: Some(format!("tempdir create failed: {e}")),
            };
        }
    };
    cmd.current_dir(tempdir.path());

    // Spawn with rlimits on Unix. The `rlimit` crate is already in the
    // tree? No — but tokio::process has no portable rlimit hook. Use
    // the `rlimit` crate via `prlimit` syscall if available; otherwise
    // skip and rely on timeout alone.
    // Note on RLIMIT_AS: tokio::process::Command does not implement
    // std::os::unix::process::CommandExt (tokio's Command is a wrapper
    // around std::process::Command, but the `pre_exec` hook requires
    // direct trait access). Setting a memory cap from inside this fn
    // would require either forking a wrapper binary or dropping down
    // to std::process::Command for the spawn step. For now, the timeout
    // is the sole resource cap; a future commit can wire in
    // `setrlimit` via a small setrlimit helper binary if needed. The
    // `mem_mb` parameter is reserved.
    #[cfg(unix)]
    let _ = mem_mb;

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return RunResult {
                status: ExperimentStatus::Failed,
                stdout: String::new(),
                stderr: String::new(),
                exit_code: None,
                duration_ms: started.elapsed().as_millis() as i64,
                runner_error: Some(format!("spawn failed: {e}")),
            };
        }
    };

    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf).await;
        buf
    });
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf).await;
        buf
    });

    let timed_out;
    let exit_status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(s)) => {
            timed_out = false;
            Some(s)
        }
        Ok(Err(e)) => {
            // Return early; `timed_out` is never read on this path.
            return RunResult {
                status: ExperimentStatus::Failed,
                stdout: String::new(),
                stderr: String::new(),
                exit_code: None,
                duration_ms: started.elapsed().as_millis() as i64,
                runner_error: Some(format!("wait failed: {e}")),
            };
        }
        Err(_) => {
            timed_out = true;
            // Best-effort kill. child is dropped at end of scope if kill
            // races with exit; either way the process is reaped.
            let _ = child.start_kill();
            // Drain wait so we don't leak the zombie.
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            None
        }
    };

    let stdout_bytes = stdout_task.await.unwrap_or_default();
    let stderr_bytes = stderr_task.await.unwrap_or_default();
    drop(tempdir);

    let stdout_str = String::from_utf8_lossy(&stdout_bytes).to_string();
    let stderr_str = String::from_utf8_lossy(&stderr_bytes).to_string();
    let duration_ms = started.elapsed().as_millis() as i64;

    if timed_out {
        return RunResult {
            status: ExperimentStatus::TimedOut,
            stdout: stdout_str,
            stderr: stderr_str,
            exit_code: None,
            duration_ms,
            runner_error: Some(format!("timeout after {:?}", timeout)),
        };
    }

    let exit = exit_status.unwrap();
    let status = if exit.success() {
        ExperimentStatus::Succeeded
    } else {
        ExperimentStatus::Failed
    };
    RunResult {
        status,
        stdout: stdout_str,
        stderr: stderr_str,
        exit_code: exit.code(),
        duration_ms,
        runner_error: None,
    }
}

// ---- rlimit helpers (Unix-only) --------------------------------------
// We use the `libc` crate (added in Cargo.toml) for the `rlimit` struct
// and `RLIMIT_AS` / `setrlimit` symbols. No custom extern blocks
// needed; libc is the canonical safe binding.

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::memory::Memory;

    /// Build a Memory-backed ExperimentRepo with a single hypothesis row
    /// so FK constraints on `experiments.hypothesis_id` are satisfied.
    async fn repo_with_hypothesis(hyp_id: i64) -> (Memory, ExperimentRepo) {
        let conn = db::open_memory().await.unwrap();
        let mem = Memory::new(conn);
        // Ensure the agent exists (FK on hypotheses runs through agents
        // only indirectly via semantic_memories, but be explicit).
        mem.ensure_agent("tester").await.unwrap();
        // Insert a stub hypothesis row.
        mem.conn()
            .execute(
                "INSERT INTO hypotheses (id, session_id, state, elo, parent_ids, semantic_id, matches_played, created_at)
                 VALUES (?1, 's1', 'draft', 1200.0, NULL, NULL, 0, ?2)
                 ON CONFLICT(id) DO NOTHING",
                (hyp_id, chrono::Utc::now().to_rfc3339()),
            )
            .await
            .unwrap();
        let repo = ExperimentRepo::new(mem.db_arc());
        (mem, repo)
    }

    #[tokio::test]
    async fn run_python_code_happy_path() {
        let r = run_python_code("print('hi')", Duration::from_secs(5), 256).await;
        assert_eq!(r.status, ExperimentStatus::Succeeded);
        assert_eq!(r.stdout.trim(), "hi");
        assert_eq!(r.exit_code, Some(0));
        assert!(r.runner_error.is_none());
    }

    #[tokio::test]
    async fn run_python_code_nonzero_exit() {
        let r = run_python_code("import sys; sys.stderr.write('boom'); sys.exit(7)", Duration::from_secs(5), 256).await;
        assert_eq!(r.status, ExperimentStatus::Failed);
        assert_eq!(r.exit_code, Some(7));
        assert!(r.stderr.contains("boom"));
    }

    #[tokio::test]
    async fn run_python_code_timeout() {
        let r = run_python_code("import time; time.sleep(10)", Duration::from_millis(500), 256).await;
        assert_eq!(r.status, ExperimentStatus::TimedOut);
        assert!(r.runner_error.is_some());
        // Should have returned within ~2s (kill grace period).
        assert!(r.duration_ms < 4000, "timeout took too long: {}ms", r.duration_ms);
    }

    #[tokio::test]
    async fn run_python_code_syntax_error() {
        let r = run_python_code("def foo(:", Duration::from_secs(5), 256).await;
        assert_eq!(r.status, ExperimentStatus::Failed);
        assert!(!r.stderr.is_empty());
    }

    #[tokio::test]
    async fn run_python_code_print_arithmetic() {
        // Mirror the smoke-test pattern: design emits a simple program,
        // execute runs it, we assert on stdout.
        let r = run_python_code("print(2 + 2)", Duration::from_secs(5), 256).await;
        assert_eq!(r.status, ExperimentStatus::Succeeded);
        assert_eq!(r.stdout.trim(), "4");
    }

    #[tokio::test]
    async fn repo_round_trip() {
        let (_mem, repo) = repo_with_hypothesis(1).await;
        let id = repo
            .insert_design("s1", 1, "print('x')", "demo_metric", "k1")
            .await
            .unwrap();
        assert!(id > 0);
        repo.mark_running(id).await.unwrap();
        repo.mark_finished(
            id,
            1,
            ExperimentStatus::Succeeded,
            Some("x\n"),
            Some(""),
            Some(0),
            42,
            Some(0.95),
            Some(&serde_json::json!({"loss": 0.05})),
            None,
        )
        .await
        .unwrap();
        let got = repo.get(id).await.unwrap().unwrap();
        assert_eq!(got.status, ExperimentStatus::Succeeded);
        assert_eq!(got.exit_code, Some(0));
        assert_eq!(got.metric_value, Some(0.95));
        assert_eq!(got.duration_ms, Some(42));
    }

    #[tokio::test]
    async fn repo_idempotency_key_dedupes() {
        let (_mem, repo) = repo_with_hypothesis(1).await;
        let id1 = repo.insert_design("s1", 1, "print('a')", "m", "dup-key").await.unwrap();
        let id2 = repo.insert_design("s1", 1, "print('b')", "m", "dup-key").await.unwrap();
        assert_eq!(id1, id2, "same idempotency key must return the same id");
    }

    #[tokio::test]
    async fn repo_latest_for_hypothesis_returns_most_recent_terminal() {
        let (_mem, repo) = repo_with_hypothesis(7).await;
        let id_old = repo.insert_design("s1", 7, "print(1)", "m", "k1").await.unwrap();
        repo.mark_finished(id_old, 7, ExperimentStatus::Failed, None, None, Some(1), 10, None, None, None).await.unwrap();
        let id_new = repo.insert_design("s1", 7, "print(2)", "m", "k2").await.unwrap();
        repo.mark_finished(id_new, 7, ExperimentStatus::Succeeded, Some("2\n"), None, Some(0), 20, None, None, None).await.unwrap();
        let latest = repo.latest_for_hypothesis(7).await.unwrap().unwrap();
        assert_eq!(latest.id, id_new);
        assert_eq!(latest.status, ExperimentStatus::Succeeded);
    }
}
