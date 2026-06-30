//! Worker loop. Claims tasks from a [`TaskQueue`], dispatches them
//! through the [`ToolRegistry`], and marks them complete/failed.
//!
//! This is the "agent loop" in code: durable, resumable, lease-protected.
//! It uses the existing memory tools (`save_semantic`, `save_behavior`,
//! `get_context`) plus any extras the registry has loaded (e.g. skills
//! from disk via the skill loader).

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures::FutureExt;
use serde_json::Value;
use tokio::sync::watch;
use tracing::{debug, error, info, warn};

use crate::memory::Memory;
use crate::queue::{EnqueueRequest, Task, TaskQueue};
use crate::registry::ToolRegistry;
use crate::tool::ToolCtx;

#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub worker_id: String,
    pub lease_seconds: i64,
    pub poll_interval: Duration,
    /// Base backoff (seconds) for failed task retries. The actual
    /// delay is `backoff * 2^(attempts-1)` with ±10% jitter, capped at
    /// 5 minutes.
    pub backoff_seconds: f64,
    /// Reclaim expired leases this often. Set to `Duration::ZERO` to
    /// disable in-loop reclaim (startup reclaim still runs).
    pub reclaim_interval: Duration,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            worker_id: format!("worker-{}", uuid::Uuid::new_v4()),
            lease_seconds: 60,
            poll_interval: Duration::from_millis(250),
            backoff_seconds: 0.5,
            reclaim_interval: Duration::from_secs(30),
        }
    }
}

/// Run the worker loop until `shutdown` flips to `true` (or the channel
/// closes). Single-threaded, single-process. Run multiple of these in
/// separate tasks for concurrency.
///
/// The loop is panic-safe: a tool that panics is caught and the task
/// is marked failed. The loop is shutdown-aware: a shutdown signal
/// causes the next claim to bail out, and an in-flight dispatch is
/// allowed to finish.
pub async fn run_worker(
    memory: Memory,
    queue: TaskQueue,
    registry: Arc<ToolRegistry>,
    config: WorkerConfig,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    // Best-effort: if reclaim fails (e.g. DB locked by supervisor at
    // startup), log and continue. The periodic reclaim in the main loop
    // will retry.
    match queue.reclaim_expired().await {
        Ok(n) if n > 0 => info!(worker = %config.worker_id, reclaimed = n, "reclaimed leases on startup"),
        Ok(_) => {}
        Err(e) => warn!(worker = %config.worker_id, error = %e, "startup reclaim failed (non-fatal)"),
    }

    info!(worker = %config.worker_id, "worker started");

    let mut last_reclaim = std::time::Instant::now();

    loop {
        if *shutdown.borrow() {
            info!(worker = %config.worker_id, "shutdown requested");
            break;
        }

        // Periodically reclaim expired leases so a slow original worker
        // doesn't keep its work forever.
        if !config.reclaim_interval.is_zero()
            && last_reclaim.elapsed() >= config.reclaim_interval
        {
            match queue.reclaim_expired().await {
                Ok(n) if n > 0 => {
                    info!(worker = %config.worker_id, reclaimed = n, "reclaimed leases");
                }
                Ok(_) => {}
                Err(e) => warn!(error = %e, "reclaim_expired failed"),
            }
            last_reclaim = std::time::Instant::now();
        }

        match queue.claim_any(&config.worker_id, config.lease_seconds).await {
            Ok(Some(task)) => {
                // Dispatch the task; failures are logged but don't kill
                // the loop.
                if let Err(e) =
                    run_one(&memory, &registry, &queue, &task, &config).await
                {
                    error!(task_id = %task.id, error = %e, "task failed");
                }
            }
            Ok(None) => {
                // Empty queue: wait for shutdown or poll interval.
                tokio::select! {
                    _ = shutdown.changed() => {}
                    _ = tokio::time::sleep(config.poll_interval) => {}
                }
            }
            Err(e) => {
                error!(error = %e, "claim error; backing off");
                tokio::select! {
                    _ = shutdown.changed() => {}
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                }
            }
        }
    }
    Ok(())
}

/// Execute a single task through the registry, then mark it complete
/// (or fail on error). Panic-safe: a tool that panics is caught and
/// the task is failed instead of aborting the worker.
async fn run_one(
    memory: &Memory,
    registry: &ToolRegistry,
    queue: &TaskQueue,
    task: &Task,
    config: &WorkerConfig,
) -> Result<()> {
    debug!(task_id = %task.id, action = %task.action, "running task");

    let ctx = ToolCtx {
        memory: memory.clone(),
        run_id: task.session_id.clone(),
        agent_name: task.agent.clone(),
    };

    // Catch panics in the tool dispatch. A panicking tool shouldn't
    // kill the worker; the task is marked failed and the loop moves on.
    let dispatch_result = std::panic::AssertUnwindSafe(registry.dispatch(
        &task.action,
        task.payload.clone(),
        &ctx,
    ))
    .catch_unwind()
    .await;

    match dispatch_result {
        Ok(Ok(_out)) => {
            queue.complete(&task.id, &config.worker_id).await?;
            Ok(())
        }
        Ok(Err(e)) => {
            // Walk the anyhow chain so the log shows the *root* cause.
            let mut chain: Vec<String> = Vec::new();
            for cause in e.chain() {
                chain.push(cause.to_string());
            }
            warn!(
                task_id = %task.id,
                action = %task.action,
                error_chain = ?chain,
                "task failed"
            );
            queue
                .fail(&task.id, &config.worker_id, &format!("{e:#}"), config.backoff_seconds)
                .await?;
            Err(e)
        }
        Err(panic) => {
            // Tool panicked. Try to extract a message, then fail the
            // task so the queue can move on.
            let msg = if let Some(s) = panic.downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = panic.downcast_ref::<String>() {
                s.clone()
            } else {
                "tool panicked".to_string()
            };
            warn!(
                task_id = %task.id,
                action = %task.action,
                panic = %msg,
                "tool panicked; failing task"
            );
            queue
                .fail(&task.id, &config.worker_id, &format!("panic: {msg}"), config.backoff_seconds)
                .await?;
            Ok(()) // Worker keeps running.
        }
    }
}

/// Convenience: enqueue a memory op as a task. Lets `main.rs` and
/// external callers schedule work without crafting the full request.
pub async fn enqueue_memory_op(
    queue: &TaskQueue,
    session_id: &str,
    agent: &str,
    action: &str,
    payload: Value,
) -> Result<String> {
    queue
        .enqueue(EnqueueRequest::new(session_id, agent, action, payload))
        .await
}

/// Helper: build a watch channel that flips to true on ctrl-c.
pub fn ctrl_c_shutdown() -> watch::Receiver<bool> {
    let (tx, rx) = watch::channel(false);
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = tx.send(true);
        }
    });
    rx
}

/// Return a (sender, receiver) pair where Ctrl+C sends `true` on the
/// sender. The receiver is what worker / consolidation watch; the sender
/// is what the supervisor uses to signal "session done, exit cleanly"
/// even without a Ctrl+C.
pub fn ctrl_c_shutdown_pair() -> (watch::Sender<bool>, watch::Receiver<bool>) {
    let (tx, rx) = watch::channel(false);
    let inner_tx = tx.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = inner_tx.send(true);
        }
    });
    (tx, rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::EnqueueRequest;
    use crate::registry::ToolRegistry;
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Test tool that records its calls into a shared `Vec`. Lets us
    /// verify the worker actually dispatches without coupling the test
    /// to the worker's internal DB state.
    #[derive(Debug)]
    struct RecorderTool {
        name: String,
        recorded: Arc<Mutex<Vec<serde_json::Value>>>,
    }

    #[async_trait::async_trait]
    impl crate::tool::Tool for RecorderTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> String {
            format!("records args into {}", self.name)
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn call(
            &self,
            args: serde_json::Value,
            _ctx: &crate::tool::ToolCtx,
        ) -> anyhow::Result<serde_json::Value> {
            self.recorded.lock().unwrap().push(args);
            Ok(serde_json::json!({"recorded": true}))
        }
    }

    fn fast_config(id: &str) -> WorkerConfig {
        WorkerConfig {
            worker_id: id.to_string(),
            lease_seconds: 5,
            poll_interval: Duration::from_millis(5),
            backoff_seconds: 0.0,
            reclaim_interval: Duration::from_secs(60),
        }
    }

    /// Run a worker with a recorder tool until `count` calls have been
    /// captured, then shut it down. Returns the recorded calls.
    async fn run_until_recorded(
        queue: TaskQueue,
        registry: Arc<ToolRegistry>,
        recorded: Arc<Mutex<Vec<serde_json::Value>>>,
        count: usize,
        worker_id: &str,
    ) -> Vec<serde_json::Value> {
        let cfg = fast_config(worker_id);
        let (tx, rx) = watch::channel(false);
        let h = tokio::spawn(async move {
            run_worker(
                crate::memory::Memory::new(crate::db::open_memory().await.unwrap()),
                queue,
                registry,
                cfg,
                rx,
            )
            .await
            .unwrap()
        });
        // Poll the recorder until `count` is hit, with a 5s timeout.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while recorded.lock().unwrap().len() < count {
            if std::time::Instant::now() > deadline {
                tx.send(true).unwrap();
                h.await.unwrap();
                panic!("worker did not record {count} calls within 5s");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tx.send(true).unwrap();
        h.await.unwrap();
        recorded.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn run_worker_dispatches_enqueued_tasks() {
        let recorded: Arc<Mutex<Vec<_>>> = Arc::new(Mutex::new(Vec::new()));
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(RecorderTool {
            name: "rec".into(),
            recorded: recorded.clone(),
        }));
        let q = TaskQueue::new(crate::db::open_memory().await.unwrap());
        for i in 0..3 {
            q.enqueue(EnqueueRequest::new(
                "s1",
                "a",
                "rec",
                json!({"i": i}),
            ))
            .await
            .unwrap();
        }
        let calls = run_until_recorded(q, Arc::new(reg), recorded, 3, "w-dispatch").await;
        assert_eq!(calls.len(), 3);
        let indices: Vec<i64> = calls.iter().map(|c| c["i"].as_i64().unwrap()).collect();
        assert_eq!(indices, vec![0, 1, 2]);
    }

    #[tokio::test]
    async fn run_worker_survives_panicking_tool() {
        #[derive(Debug)]
        struct PanicTool;
        #[async_trait::async_trait]
        impl crate::tool::Tool for PanicTool {
            fn name(&self) -> &str {
                "panicker"
            }
            fn description(&self) -> String {
                "always panics".into()
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            async fn call(
                &self,
                _a: serde_json::Value,
                _c: &crate::tool::ToolCtx,
            ) -> anyhow::Result<serde_json::Value> {
                panic!("intentional panic for test");
            }
        }

        let recorded: Arc<Mutex<Vec<_>>> = Arc::new(Mutex::new(Vec::new()));
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(PanicTool));
        reg.register(Arc::new(RecorderTool {
            name: "rec".into(),
            recorded: recorded.clone(),
        }));
        let q = TaskQueue::new(crate::db::open_memory().await.unwrap());
        // First task panics; the worker must survive and process
        // the second task normally.
        q.enqueue(EnqueueRequest::new("s1", "a", "panicker", json!({})))
            .await
            .unwrap();
        q.enqueue(EnqueueRequest::new("s1", "a", "rec", json!({"after": "panic"})))
            .await
            .unwrap();

        let cfg = fast_config("w-panic");
        let (tx, rx) = watch::channel(false);
        let h = tokio::spawn(async move {
            run_worker(
                crate::memory::Memory::new(crate::db::open_memory().await.unwrap()),
                q,
                Arc::new(reg),
                cfg,
                rx,
            )
            .await
            .unwrap()
        });

        // Wait for the recorder to see the second call.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while recorded.lock().unwrap().is_empty() {
            if std::time::Instant::now() > deadline {
                tx.send(true).unwrap();
                h.await.unwrap();
                panic!("worker did not process the post-panic task");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tx.send(true).unwrap();
        // The critical assertion: the worker task didn't propagate
        // the panic. If `run_one` didn't catch_unwind, this would be
        // a JoinError.
        h.await.expect("worker task panicked; panic-safety failed");
        let calls = recorded.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["after"], "panic");
    }

    #[tokio::test]
    async fn run_worker_backs_off_after_failure() {
        // A failing tool (returns Err, not panics) should be retried
        // up to max_attempts, then dead-lettered.
        #[derive(Debug)]
        struct FailTool {
            calls: Arc<Mutex<u32>>,
        }
        #[async_trait::async_trait]
        impl crate::tool::Tool for FailTool {
            fn name(&self) -> &str {
                "fail"
            }
            fn description(&self) -> String {
                "always fails".into()
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            async fn call(
                &self,
                _a: serde_json::Value,
                _c: &crate::tool::ToolCtx,
            ) -> anyhow::Result<serde_json::Value> {
                *self.calls.lock().unwrap() += 1;
                Err(anyhow::anyhow!("intentional failure"))
            }
        }

        let calls = Arc::new(Mutex::new(0u32));
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(FailTool {
            calls: calls.clone(),
        }));
        let q = TaskQueue::new(crate::db::open_memory().await.unwrap());
        let _id = q
            .enqueue({
                let mut req = EnqueueRequest::new("s1", "a", "fail", json!({}));
                req.max_attempts = 2;
                req
            })
            .await
            .unwrap();
        let cfg = fast_config("w-backoff");
        // Zero backoff so the test is fast.
        let (tx, rx) = watch::channel(false);
        let h = tokio::spawn(async move {
            run_worker(
                crate::memory::Memory::new(crate::db::open_memory().await.unwrap()),
                q,
                Arc::new(reg),
                cfg,
                rx,
            )
            .await
            .unwrap()
        });
        // Wait for exactly 2 calls. With backoff = 0, this happens
        // fast (1st fail → requeue → 2nd fail → dead).
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while *calls.lock().unwrap() < 2 {
            if std::time::Instant::now() > deadline {
                tx.send(true).unwrap();
                h.await.unwrap();
                panic!("expected 2 calls, got {}", *calls.lock().unwrap());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tx.send(true).unwrap();
        h.await.unwrap();
        // max_attempts=2 means: 1st attempt fails → requeue →
        // 2nd attempt fails → dead. So the tool is called exactly 2x.
        assert_eq!(*calls.lock().unwrap(), 2);
    }
}
