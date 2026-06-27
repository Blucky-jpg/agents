//! `events` and `sessions` table operations: log_event, ensure_session,
//! recent_marker_errors.
//!
//! Trace data (`rendered_prompt`, `raw_response`) is optional and only the
//! runner-loop path fills it in; the simpler `log_event` wrapper stays for
//! backward compat.

use anyhow::{Context as _, Result};
use chrono::Utc;
use serde_json::Value;

use super::{helpers::idempotency_key, types::Event, Memory, MemoryError};
use crate::bus::MemoryEvent;

impl Memory {
    /// Auto-log a single turn. Called by the runner before/after each
    /// `Claude::query` — this is the closest we can get to a hook from
    /// outside the ante runtime.
    ///
    /// Idempotent on `(run_id, agent_name, step_index, type, sha256(payload))`.
    /// Re-running with identical inputs is a no-op.
    pub async fn log_event(
        &self,
        run_id: &str,
        agent_name: &str,
        step_index: i64,
        event_type: &str,
        payload: Option<Value>,
    ) -> Result<(), MemoryError> {
        // Backward-compatible wrapper — no trace data.
        self.log_event_with_trace(run_id, agent_name, step_index, event_type, payload, None, None)
            .await
    }

    /// Extended event logger that also persists the rendered prompt and
    /// raw LLM response. Use this from the agent loop for traceability.
    pub async fn log_event_with_trace(
        &self,
        run_id: &str,
        agent_name: &str,
        step_index: i64,
        event_type: &str,
        payload: Option<Value>,
        rendered_prompt: Option<&str>,
        raw_response: Option<&str>,
    ) -> Result<(), MemoryError> {
        let agent_id = self.ensure_agent(agent_name).await?;
        self.ensure_session(run_id, agent_id).await?;
        let payload_str = payload
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .context("serializing event payload")?;
        let key = idempotency_key(&[
            "event",
            run_id,
            &agent_id.to_string(),
            &step_index.to_string(),
            event_type,
            payload_str.as_deref().unwrap_or(""),
        ]);
        let now = Utc::now().to_rfc3339();
        // ON CONFLICT DO NOTHING + RETURNING id: if the row was newly
        // inserted, we get the id back; on conflict, the result set is
        // empty. This lets us publish *only* on actual inserts and skip
        // the bus on retries.
        let mut rows = self
            .conn()
            .query(
                "INSERT INTO events (run_id, agent_id, step_index, type, payload_json, rendered_prompt, raw_response, created_at, idempotency_key)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(idempotency_key) DO NOTHING
                 RETURNING id",
                (
                    run_id,
                    agent_id,
                    step_index,
                    event_type,
                    payload_str,
                    rendered_prompt,
                    raw_response,
                    now,
                    key,
                ),
            )
            .await
            .context("inserting event")?;
        if let Some(row) = rows.next().await? {
            let id: i64 = row.get(0)?;
            self.bus.publish(MemoryEvent::EventLogged {
                id,
                run_id: run_id.to_string(),
                agent: agent_name.to_string(),
                type_: event_type.to_string(),
                payload,
            });
        }
        Ok(())
    }

    /// Ensure a `sessions` row exists for `(run_id, agent_id)`. Creates one
    /// on first call for the pair.
    pub async fn ensure_session(&self, run_id: &str, agent_id: i64) -> Result<(), MemoryError> {
        self.db
            .conn()
            .execute(
                "INSERT OR IGNORE INTO sessions (run_id, agent_id, started_at) VALUES (?1, ?2, ?3)",
                (run_id, agent_id, Utc::now().to_rfc3339()),
            )
            .await
            .context("ensuring session")?;
        Ok(())
    }

    /// Load recent marker errors for an agent. Used to inject feedback
    /// into the next turn's system prompt so the LLM can self-correct.
    pub async fn recent_marker_errors(
        &self,
        run_id: &str,
        agent_name: &str,
        limit: usize,
    ) -> Vec<(String, String)> {
        let result = self
            .conn()
            .query(
                "SELECT json_extract(payload_json, '$.op') as op, json_extract(payload_json, '$.error') as error
                 FROM events
                 WHERE run_id = ?1 AND type = 'memory_op_failed'
                   AND json_extract(payload_json, '$.agent') = ?2
                 ORDER BY id DESC LIMIT ?3",
                (run_id, agent_name, limit as i64),
            )
            .await;
        let mut rows = match result {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.unwrap_or(None) {
            let op: Option<String> = row.get(0).unwrap_or(None);
            let err: Option<String> = row.get(1).unwrap_or(None);
            if let (Some(o), Some(e)) = (op, err) {
                out.push((o, e));
            }
        }
        out
    }

    /// Build [`Event`] rows from the `events` table for a given run.
    /// Used by [`super::super::context::get_context`] to populate
    /// `Context::recent_events`.
    pub(super) async fn recent_events_for_run(
        &self,
        run_id: &str,
        limit: usize,
    ) -> Result<Vec<Event>, MemoryError> {
        let sql = "SELECT id, run_id, agent_id, step_index, type, payload_json, created_at
                   FROM events
                   WHERE run_id = ?1
                   ORDER BY id DESC
                   LIMIT ?2";
        let mut rows = self
            .conn()
            .query(sql, (run_id, limit as i64))
            .await
            .context("querying events")?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            let payload_str: Option<String> = row.get(5)?;
            let payload = payload_str
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .context("parsing event payload")?;
            out.push(Event {
                id: row.get(0)?,
                run_id: row.get(1)?,
                agent_id: row.get(2)?,
                step_index: row.get(3)?,
                r#type: row.get(4)?,
                payload,
                created_at: row.get(6)?,
            });
        }
        Ok(out)
    }
}