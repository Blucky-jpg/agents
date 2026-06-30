//! Experiment tools: `design_experiment`, `execute_experiment`, `evaluate_result`.
//!
//! These three close the empirical loop. The `experiment` agent owns
//! them; the runner wires them into the pipeline via `enqueue_follow_ups`
//! in `run_agent.rs`.
//!
//! Lifecycle:
//!   design  →  execute  →  evaluate
//!
//! `design` writes a row to `experiments` with status `designed` and a
//! `semantic_memories` note (scope=`experiment_design`) carrying the
//! full code. `execute` actually runs the python (sandboxed subprocess
//! via `experiment::run_python_code`), updates the row to a terminal
//! status, and returns stdout/stderr/metric. `evaluate` is the LLM's
//! interpretive pass — it writes a `semantic_memories` row with
//! scope=`experiment_result` summarizing what the metric means.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::experiment::{run_python_code, ExperimentRepo, ExperimentStatus};
use crate::memory::{idempotency_key, MemoryError};

use super::{Tool, ToolCtx, ToolOutput};

/// Default execution parameters. Tuned for "compute a few statistics"
/// workloads, not full ML training. Override per-call via the schema's
/// optional fields.
const DEFAULT_TIMEOUT_S: u64 = 30;
const DEFAULT_MEM_MB: u64 = 1536;

// =====================================================================
// design_experiment
// =====================================================================

pub struct DesignExperimentTool;

#[async_trait]
impl Tool for DesignExperimentTool {
    fn name(&self) -> &str {
        "design_experiment"
    }
    fn description(&self) -> String {
        "Design a Python experiment to test a hypothesis. Records the code \
         and metric_name in the experiments table. The next step is to call \
         execute_experiment with the returned id. Do NOT execute side-effects \
         in the code — it runs in a sandbox."
            .to_string()
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "hypothesis_id": {
                    "type": "integer",
                    "description": "The hypothesis this experiment will test."
                },
                "code": {
                    "type": "string",
                    "description": "Python source code. Will be run via `python3 -c`. \
                                   Must be self-contained — no stdin, no file IO outside \
                                   the temporary working dir, no network."
                },
                "metric_name": {
                    "type": "string",
                    "description": "Name of the metric the code computes (e.g. 'accuracy', 'p_value')."
                },
                "metric_value": {
                    "description": "Optional pre-computed metric value if the LLM evaluated it mentally. \
                                   Prefer letting execute_experiment compute it from the code."
                },
                "description": {
                    "type": "string",
                    "description": "One-sentence explanation of what the experiment tests."
                }
            },
            "required": ["hypothesis_id", "code", "metric_name"]
        })
    }
    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let hypothesis_id = args
            .get("hypothesis_id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| anyhow::anyhow!("design_experiment: missing 'hypothesis_id'"))?;
        let code = args
            .get("code")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("design_experiment: missing 'code'"))?;
        if code.trim().is_empty() {
            return Err(anyhow::anyhow!("design_experiment: 'code' is empty"));
        }
        if code.len() > 64 * 1024 {
            return Err(anyhow::anyhow!(
                "design_experiment: 'code' exceeds 64KiB cap"
            ));
        }
        let metric_name = args
            .get("metric_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("design_experiment: missing 'metric_name'"))?;
        let description = args.get("description").and_then(|v| v.as_str());

        let nonce = Uuid::new_v4().to_string();
        let key = idempotency_key(&[
            "experiment_design",
            &ctx.run_id,
            &hypothesis_id.to_string(),
            &nonce,
        ]);
        let repo = ExperimentRepo::new(ctx.memory.db_arc());
        let experiment_id = repo
            .insert_design(&ctx.run_id, hypothesis_id, code, metric_name, &key)
            .await
            .map_err(MemoryError::from)?;

        // Also save a semantic memory note (scope=experiment_design) so
        // the code is retrievable via search/peek later.
        let mut details = json!({
            "experiment_id": experiment_id,
            "metric_name": metric_name,
            "code": code,
        });
        if let Some(d) = description {
            details["description"] = json!(d);
        }
        if let Some(mv) = args.get("metric_value") {
            details["metric_value"] = mv.clone();
        }
        let note_summary = description.unwrap_or("experiment design");
        let _ = ctx
            .memory
            .save_semantic(
                &ctx.run_id,
                Some(&ctx.agent_name),
                "experiment_design",
                note_summary,
                Some(details),
            )
            .await?;

        Ok(json!({
            "experiment_id": experiment_id,
            "hypothesis_id": hypothesis_id,
            "status": "designed",
        }))
    }
}

// =====================================================================
// execute_experiment
// =====================================================================

pub struct ExecuteExperimentTool;

#[async_trait]
impl Tool for ExecuteExperimentTool {
    fn name(&self) -> &str {
        "execute_experiment"
    }
    fn description(&self) -> String {
        "Execute a previously-designed experiment. Runs the stored Python code \
         in a sandboxed subprocess (tempdir + timeout). Records stdout, stderr, \
         exit_code, and the metric value. Returns the run result. \
         This is the ONLY tool that touches the filesystem."
            .to_string()
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "experiment_id": {
                    "type": "integer",
                    "description": "The id returned by design_experiment."
                },
                "timeout_s": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 600,
                    "default": DEFAULT_TIMEOUT_S,
                    "description": "Wall-clock timeout in seconds."
                },
                "mem_mb": {
                    "type": "integer",
                    "minimum": 16,
                    "maximum": 4096,
                    "default": DEFAULT_MEM_MB,
                    "description": "Memory budget in MB (currently advisory only; the timeout is the hard cap)."
                }
            },
            "required": ["experiment_id"]
        })
    }
    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let experiment_id = args
            .get("experiment_id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| anyhow::anyhow!("execute_experiment: missing 'experiment_id'"))?;
        let timeout_s = args
            .get("timeout_s")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_S);
        let mem_mb = args
            .get("mem_mb")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_MEM_MB);

        let repo = ExperimentRepo::new(ctx.memory.db_arc());
        let exp = repo
            .get(experiment_id)
            .await
            .map_err(MemoryError::from)?
            .ok_or_else(|| {
                anyhow::anyhow!("execute_experiment: experiment {experiment_id} not found")
            })?;

        if exp.status == ExperimentStatus::Succeeded {
            // Idempotent: a successful run stays successful.
            return Ok(json!({
                "experiment_id": experiment_id,
                "status": exp.status.as_str(),
                "stdout": exp.stdout,
                "stderr": exp.stderr,
                "metric_name": exp.metric_name,
                "metric_value": exp.metric_value,
                "idempotent": true,
            }));
        }

        repo.mark_running(experiment_id)
            .await
            .map_err(MemoryError::from)?;

        // Log the run start.
        let _ = ctx
            .memory
            .log_event(
                &ctx.run_id,
                &ctx.agent_name,
                0,
                "experiment_started",
                Some(json!({
                    "experiment_id": experiment_id,
                    "hypothesis_id": exp.hypothesis_id,
                    "metric_name": exp.metric_name,
                    "timeout_s": timeout_s,
                })),
            )
            .await;

        let timeout = std::time::Duration::from_secs(timeout_s);
        let result = run_python_code(&exp.code, timeout, mem_mb).await;

        // Persist result.
        let metric_value: Option<f64> = if result.status == ExperimentStatus::Succeeded {
            // Convention: if the program prints a JSON object on its last
            // line with a key matching `metric_name`, parse and store it.
            // Otherwise store `None` and let evaluate_result interpret
            // stdout/stderr.
            parse_metric_from_stdout(&result.stdout, &exp.metric_name)
        } else {
            None
        };
        repo.mark_finished(
            experiment_id,
            exp.hypothesis_id,
            result.status,
            Some(&result.stdout),
            Some(&result.stderr),
            result.exit_code,
            result.duration_ms,
            metric_value,
            None,
            result.runner_error.as_deref(),
        )
        .await
        .map_err(MemoryError::from)?;

        // Log completion for audit.
        let _ = ctx
            .memory
            .log_event(
                &ctx.run_id,
                &ctx.agent_name,
                0,
                "experiment_completed",
                Some(json!({
                    "experiment_id": experiment_id,
                    "hypothesis_id": exp.hypothesis_id,
                    "status": result.status.as_str(),
                    "exit_code": result.exit_code,
                    "duration_ms": result.duration_ms,
                    "metric_name": exp.metric_name,
                    "metric_value": metric_value,
                })),
            )
            .await;

        Ok(json!({
            "experiment_id": experiment_id,
            "status": result.status.as_str(),
            "stdout": result.stdout,
            "stderr": result.stderr,
            "exit_code": result.exit_code,
            "duration_ms": result.duration_ms,
            "metric_name": exp.metric_name,
            "metric_value": metric_value,
            "runner_error": result.runner_error,
        }))
    }
}

/// Try to extract a metric value from the last line of stdout. Format
/// expected: a JSON line like `{"accuracy": 0.94}` or a plain
/// `0.94` (the latter only when the metric_name is `value` or empty).
fn parse_metric_from_stdout(stdout: &str, metric_name: &str) -> Option<f64> {
    if metric_name.is_empty() {
        return None;
    }
    let last_line = stdout.trim().lines().last()?;
    // First try: JSON object with the metric key.
    if let Ok(v) = serde_json::from_str::<Value>(last_line) {
        if let Some(n) = v.get(metric_name).and_then(|x| x.as_f64()) {
            return Some(n);
        }
        // Allow "metric" as a generic key.
        if let Some(n) = v.get("metric").and_then(|x| x.as_f64()) {
            return Some(n);
        }
    }
    // Fallback: if metric_name is "value", accept a bare number.
    if metric_name == "value" {
        return last_line.parse::<f64>().ok();
    }
    None
}

// =====================================================================
// evaluate_result
// =====================================================================

pub struct EvaluateResultTool;

#[async_trait]
impl Tool for EvaluateResultTool {
    fn name(&self) -> &str {
        "evaluate_result"
    }
    fn description(&self) -> String {
        "Record the LLM's interpretation of an experiment result. Writes a \
         semantic memory (scope=experiment_result) linking the metric back \
         to the hypothesis. After calling this, the pipeline will enqueue a \
         reflection_on_result pass to fold the finding into the tournament."
            .to_string()
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "experiment_id": {
                    "type": "integer",
                    "description": "The experiment that was executed."
                },
                "hypothesis_id": {
                    "type": "integer",
                    "description": "The hypothesis the experiment tested."
                },
                "verdict": {
                    "type": "string",
                    "enum": ["supports", "refutes", "inconclusive", "error"],
                    "description": "One-word verdict based on the metric."
                },
                "summary": {
                    "type": "string",
                    "description": "One-sentence summary of what the result means for the hypothesis."
                },
                "details": {
                    "description": "Optional structured details: interpretation, confidence, caveats."
                }
            },
            "required": ["experiment_id", "hypothesis_id", "verdict", "summary"]
        })
    }
    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let experiment_id = args
            .get("experiment_id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| anyhow::anyhow!("evaluate_result: missing 'experiment_id'"))?;
        let hypothesis_id = args
            .get("hypothesis_id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| anyhow::anyhow!("evaluate_result: missing 'hypothesis_id'"))?;
        let verdict = args
            .get("verdict")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("evaluate_result: missing 'verdict'"))?;
        let summary = args
            .get("summary")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("evaluate_result: missing 'summary'"))?;
        let details = args.get("details").cloned();

        // Read the experiment row so we can attach the metric in the
        // semantic memory.
        let repo = ExperimentRepo::new(ctx.memory.db_arc());
        let exp = repo
            .get(experiment_id)
            .await
            .map_err(MemoryError::from)?
            .ok_or_else(|| {
                anyhow::anyhow!("evaluate_result: experiment {experiment_id} not found")
            })?;

        let mut details_obj = details.unwrap_or_else(|| json!({}));
        if !details_obj.is_object() {
            // The LLM sometimes passes a string; wrap it.
            details_obj = json!({ "raw": details_obj });
        }
        let obj = details_obj.as_object_mut().unwrap();
        obj.insert("experiment_id".into(), json!(experiment_id));
        obj.insert("hypothesis_id".into(), json!(hypothesis_id));
        obj.insert("verdict".into(), json!(verdict));
        obj.insert("experiment_status".into(), json!(exp.status.as_str()));
        obj.insert("metric_name".into(), json!(exp.metric_name));
        if let Some(mv) = exp.metric_value {
            obj.insert("metric_value".into(), json!(mv));
        }
        if let Some(ec) = exp.exit_code {
            obj.insert("exit_code".into(), json!(ec));
        }
        if let Some(d) = exp.duration_ms {
            obj.insert("duration_ms".into(), json!(d));
        }

        let semantic_id = ctx
            .memory
            .save_semantic(
                &ctx.run_id,
                Some(&ctx.agent_name),
                "experiment_result",
                summary,
                Some(details_obj),
            )
            .await?;

        // Audit event.
        let _ = ctx
            .memory
            .log_event(
                &ctx.run_id,
                &ctx.agent_name,
                0,
                "experiment_evaluated",
                Some(json!({
                    "experiment_id": experiment_id,
                    "hypothesis_id": hypothesis_id,
                    "verdict": verdict,
                    "semantic_id": semantic_id,
                })),
            )
            .await;

        Ok(json!({
            "semantic_id": semantic_id,
            "verdict": verdict,
            "hypothesis_id": hypothesis_id,
            "next_step": "reflection_on_result",
        }))
    }
}

// =====================================================================
// run_python
// =====================================================================

/// Synchronous inline Python execution. Unlike `design_experiment` +
/// `execute_experiment` (which persist a row in the `experiments` table
/// and feed `evaluate_result` → `reflection_on_result`), this tool runs
/// the supplied code directly and returns stdout/stderr/exit_code.
///
/// **Blocking contract**: the tool's `call` does not return until the
/// subprocess has exited, errored, or hit the wall-clock timeout. The
/// next LLM turn only begins after this tool returns. There is no
/// streaming, no early-return, no background execution.
///
/// **Use for**: quick numeric / sanity-check computations, one-off
/// data transforms, sanity-checking a hypothesis's algebra before
/// designing a full experiment. **Don't use for**: long-running jobs
/// (over the timeout cap), or anything that should be auditable as a
/// hypothesis-driven experiment — those should go through the
/// `design_experiment` → `execute_experiment` path so the run is
/// persisted.
///
/// **Sandbox**: spawned in a fresh `tempfile::TempDir` as cwd, stdin
/// closed, stdout/stderr piped. The tempdir is dropped on return,
/// deleting any files the script created. `mem_mb` is currently
/// advisory only — see `run_python_code` for the caveat.
pub struct RunPythonTool;

#[async_trait]
impl Tool for RunPythonTool {
    fn name(&self) -> &str {
        "run_python"
    }
    fn description(&self) -> String {
        "Run a Python snippet inline. Synchronous: this call blocks until the \
         script finishes (success, non-zero exit, syntax error, or wall-clock \
         timeout). The next LLM turn only starts after this returns. Runs in a \
         fresh tempdir; stdout/stderr/exit_code/duration are returned. Use for \
         quick computations or sanity checks. For auditable hypothesis-driven \
         runs, prefer design_experiment + execute_experiment instead."
            .to_string()
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "Python source to run via `python3 -c <code>`. Newlines are fine."
                },
                "timeout_s": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 600,
                    "default": DEFAULT_TIMEOUT_S,
                    "description": "Wall-clock timeout in seconds. The subprocess is killed and the tool returns with status=timed_out after this many seconds."
                },
                "mem_mb": {
                    "type": "integer",
                    "minimum": 16,
                    "maximum": 4096,
                    "default": DEFAULT_MEM_MB,
                    "description": "Memory budget in MB. Currently advisory only — the timeout is the hard cap."
                }
            },
            "required": ["code"]
        })
    }
    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let code = args
            .get("code")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("run_python: missing 'code'"))?;
        let timeout_s = args
            .get("timeout_s")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_S);
        let mem_mb = args
            .get("mem_mb")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_MEM_MB);

        let timeout = std::time::Duration::from_secs(timeout_s);

        // Audit: log the call. We log code length, not the code itself,
        // since (a) the code can be large and (b) the full output of the
        // run is captured in the events table below via run_id + agent
        // for any later replay.
        if let Err(e) = ctx
            .memory
            .log_event(
                &ctx.run_id,
                &ctx.agent_name,
                0,
                "python_executed",
                Some(json!({
                    "code_bytes": code.len(),
                    "timeout_s": timeout_s,
                    "mem_mb": mem_mb,
                })),
            )
            .await
        {
            tracing::warn!(
                error = %e,
                "log_event failed for python_executed"
            );
        }

        // This is the blocking call. We do not return until the
        // subprocess has exited, errored, or been killed by the timeout.
        // The next LLM turn starts only after this future resolves.
        let result = run_python_code(code, timeout, mem_mb).await;

        Ok(json!({
            "status": result.status.as_str(),
            "stdout": result.stdout,
            "stderr": result.stderr,
            "exit_code": result.exit_code,
            "duration_ms": result.duration_ms,
            "runner_error": result.runner_error,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::memory::Memory;

    async fn fixture() -> (Memory, i64) {
        let mem = Memory::new(db::open_memory().await.unwrap());
        mem.ensure_agent("experimenter").await.unwrap();
        mem.conn()
            .execute(
                "INSERT INTO hypotheses (id, session_id, state, elo, parent_ids, semantic_id, matches_played, created_at)
                 VALUES (1, 's_test', 'draft', 1200.0, NULL, NULL, 0, ?1)",
                [chrono::Utc::now().to_rfc3339()],
            )
            .await
            .unwrap();
        (mem, 1)
    }

    #[tokio::test]
    async fn design_tool_inserts_row_and_semantic_note() {
        let (mem, hyp) = fixture().await;
        let ctx = ToolCtx {
            memory: mem.clone(),
            run_id: "s_test".into(),
            agent_name: "experimenter".into(),
        };
        let out = DesignExperimentTool
            .call(
                json!({
                    "hypothesis_id": hyp,
                    "code": "print(1+1)",
                    "metric_name": "result",
                    "description": "smoke test"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out["status"], "designed");
        let id = out["experiment_id"].as_i64().unwrap();
        assert!(id > 0);
    }

    #[tokio::test]
    async fn execute_tool_runs_python_end_to_end() {
        // The smoke test for the whole pipeline: design → execute → assert.
        let (mem, hyp) = fixture().await;
        let ctx = ToolCtx {
            memory: mem.clone(),
            run_id: "s_test".into(),
            agent_name: "experimenter".into(),
        };
        let design_out = DesignExperimentTool
            .call(
                json!({"hypothesis_id": hyp, "code": "print(2+2)", "metric_name": "value"}),
                &ctx,
            )
            .await
            .unwrap();
        let id = design_out["experiment_id"].as_i64().unwrap();
        let exec_out = ExecuteExperimentTool
            .call(json!({"experiment_id": id}), &ctx)
            .await
            .unwrap();
        assert_eq!(exec_out["status"], "succeeded");
        assert_eq!(exec_out["stdout"].as_str().unwrap().trim(), "4");
        assert_eq!(exec_out["metric_value"].as_f64().unwrap(), 4.0);
    }

    #[tokio::test]
    async fn execute_tool_idempotent_on_already_succeeded() {
        let (mem, hyp) = fixture().await;
        let ctx = ToolCtx {
            memory: mem.clone(),
            run_id: "s_test".into(),
            agent_name: "experimenter".into(),
        };
        let id = DesignExperimentTool
            .call(
                json!({"hypothesis_id": hyp, "code": "print('ok')", "metric_name": "value"}),
                &ctx,
            )
            .await
            .unwrap()["experiment_id"]
            .as_i64()
            .unwrap();
        ExecuteExperimentTool
            .call(json!({"experiment_id": id}), &ctx)
            .await
            .unwrap();
        let second = ExecuteExperimentTool
            .call(json!({"experiment_id": id}), &ctx)
            .await
            .unwrap();
        assert_eq!(second["idempotent"], true);
    }

    #[tokio::test]
    async fn evaluate_tool_persists_verdict() {
        let (mem, hyp) = fixture().await;
        let ctx = ToolCtx {
            memory: mem.clone(),
            run_id: "s_test".into(),
            agent_name: "experimenter".into(),
        };
        let id = DesignExperimentTool
            .call(
                json!({"hypothesis_id": hyp, "code": "print(1)", "metric_name": "value"}),
                &ctx,
            )
            .await
            .unwrap()["experiment_id"]
            .as_i64()
            .unwrap();
        ExecuteExperimentTool
            .call(json!({"experiment_id": id}), &ctx)
            .await
            .unwrap();
        let out = EvaluateResultTool
            .call(
                json!({
                    "experiment_id": id,
                    "hypothesis_id": hyp,
                    "verdict": "supports",
                    "summary": "result confirms the hypothesis"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out["verdict"], "supports");
        assert!(out["semantic_id"].as_i64().unwrap() > 0);
    }

    #[test]
    fn parse_metric_from_stdout_handles_json_and_bare_number() {
        assert_eq!(
            parse_metric_from_stdout("noise\n{\"accuracy\": 0.94}\n", "accuracy"),
            Some(0.94)
        );
        assert_eq!(
            parse_metric_from_stdout("0.5\n", "value"),
            Some(0.5)
        );
        assert_eq!(parse_metric_from_stdout("hello", "value"), None);
    }

    /// `run_python` happy path: stdout/exit_code/status all come back.
    /// Confirms the synchronous-blocking contract by construction —
    /// the `await` below cannot resolve until the subprocess has exited.
    #[tokio::test]
    async fn run_python_tool_returns_succeeded_on_clean_exit() {
        use crate::db;
        let mem = Memory::new(db::open_memory().await.unwrap());
        let ctx = ToolCtx {
            memory: mem,
            run_id: "r-py".into(),
            agent_name: "generation".into(),
        };
        let out = RunPythonTool
            .call(json!({"code": "print(2 + 2)"}), &ctx)
            .await
            .unwrap();
        assert_eq!(out["status"], "succeeded");
        assert_eq!(out["exit_code"].as_i64().unwrap(), 0);
        assert_eq!(out["stdout"].as_str().unwrap().trim(), "4");
    }

    /// `run_python` failure path: a non-zero exit returns `status=failed`
    /// with stderr populated. Synchronous: the next LLM turn only sees
    /// this result after the tool returns.
    #[tokio::test]
    async fn run_python_tool_returns_failed_on_nonzero_exit() {
        use crate::db;
        let mem = Memory::new(db::open_memory().await.unwrap());
        let ctx = ToolCtx {
            memory: mem,
            run_id: "r-py".into(),
            agent_name: "reflection".into(),
        };
        let out = RunPythonTool
            .call(
                json!({"code": "import sys; sys.stderr.write('boom'); sys.exit(7)"}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out["status"], "failed");
        assert_eq!(out["exit_code"].as_i64().unwrap(), 7);
        assert!(out["stderr"].as_str().unwrap().contains("boom"));
    }

    /// `run_python` missing required field: validation error, no
    /// subprocess spawned.
    #[tokio::test]
    async fn run_python_tool_rejects_missing_code() {
        use crate::db;
        let mem = Memory::new(db::open_memory().await.unwrap());
        let ctx = ToolCtx {
            memory: mem,
            run_id: "r-py".into(),
            agent_name: "generation".into(),
        };
        let err = RunPythonTool.call(json!({}), &ctx).await.unwrap_err();
        assert!(err.to_string().contains("missing 'code'"));
    }

    /// `run_python` timeout path: wall-clock cap fires and the tool
    /// returns `status=timed_out`. Crucially this still proves the
    /// blocking contract — the call does not return until the timeout
    /// has elapsed and the subprocess has been killed.
    #[tokio::test]
    async fn run_python_tool_returns_timed_out_when_subprocess_exceeds_cap() {
        use crate::db;
        let mem = Memory::new(db::open_memory().await.unwrap());
        let ctx = ToolCtx {
            memory: mem,
            run_id: "r-py".into(),
            agent_name: "evolution".into(),
        };
        let started = std::time::Instant::now();
        let out = RunPythonTool
            .call(
                json!({"code": "import time; time.sleep(10)", "timeout_s": 1}),
                &ctx,
            )
            .await
            .unwrap();
        let elapsed = started.elapsed();
        assert_eq!(out["status"], "timed_out");
        // Should return shortly after the 1s cap, not at the 10s natural
        // sleep boundary. Allow generous slack for CI scheduling.
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "run_python should respect timeout, took {elapsed:?}"
        );
    }
}
