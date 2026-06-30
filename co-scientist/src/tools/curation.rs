//! Destructive curation tools: archive (soft-delete) and hard-delete.
//! Both log an `observation_archived` / `observation_deleted` event for
//! audit. Hard-delete is gated to `kind=behavior` and requires an
//! `evidence` trail per IMPROVEMENT_PLAN §2.1.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use super::{Tool, ToolCtx, ToolOutput};

/// Tool: archive an observation (soft-delete). For `kind=semantic`,
/// this calls `memory.archive_semantic`; for `kind=behavior`,
/// `memory.archive_behavior`. Archived rows are filtered out of all
/// retrieval paths.
pub struct ArchiveObservationTool;

#[async_trait]
impl Tool for ArchiveObservationTool {
    fn name(&self) -> &str {
        "archive_observation"
    }
    fn description(&self) -> String {
        "Archive an observation (soft-delete). Archived rows are hidden from \
         all retrieval but kept for audit. Use `kind=semantic` or `kind=behavior`."
            .to_string()
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["semantic", "behavior"],
                    "description": "Which table to archive from."
                },
                "id": {
                    "type": "integer",
                    "description": "The observation id to archive."
                },
                "reason": {
                    "type": "string",
                    "description": "Optional audit note explaining why this was archived."
                }
            },
            "required": ["kind", "id"]
        })
    }
    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let kind = args
            .get("kind")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("archive_observation: missing 'kind'"))?;
        let id = args
            .get("id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| anyhow::anyhow!("archive_observation: missing 'id'"))?;
        let reason = args.get("reason").and_then(|v| v.as_str()).unwrap_or("");
        match kind {
            "semantic" => ctx.memory.archive_semantic(id).await?,
            "behavior" => ctx.memory.archive_behavior(id).await?,
            other => {
                return Err(anyhow::anyhow!(
                    "archive_observation: invalid kind '{other}'"
                ));
            }
        }
        // Log the archive as an event for traceability.
        if let Err(e) = ctx
            .memory
            .log_event(
                &ctx.run_id,
                &ctx.agent_name,
                0,
                "observation_archived",
                Some(serde_json::json!({
                    "kind": kind,
                    "id": id,
                    "reason": reason,
                })),
            )
            .await
        {
            tracing::warn!(
                kind = %kind,
                id = id,
                error = %e,
                "log_event failed for observation_archived"
            );
        }
        Ok(serde_json::json!({ "archived": true, "kind": kind, "id": id }))
    }
}

/// Tool: hard-delete a behavior observation. Requires `evidence` (audit
/// trail) per the IMPROVEMENT_PLAN §2.1 risk mitigation. Semantic
/// memories cannot be hard-deleted (use `archive_observation` instead)
/// — losing them entirely would lose the audit trail.
pub struct DeleteObservationTool;

#[async_trait]
impl Tool for DeleteObservationTool {
    fn name(&self) -> &str {
        "delete_observation"
    }
    fn description(&self) -> String {
        "Hard-delete a behavior observation. Requires 'evidence' — a list \
         of event ids that justify the deletion. Only allowed for \
         kind=behavior; for kind=semantic use archive_observation."
            .to_string()
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["behavior"],
                    "description": "Only 'behavior' is accepted."
                },
                "id": {
                    "type": "integer",
                    "description": "The behavior observation id to delete."
                },
                "evidence": {
                    "description": "Audit trail: list of event ids that triggered this deletion.",
                    "type": "array",
                    "items": { "type": "integer" },
                    "minItems": 1
                }
            },
            "required": ["kind", "id", "evidence"]
        })
    }
    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let kind = args
            .get("kind")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("delete_observation: missing 'kind'"))?;
        if kind != "behavior" {
            return Err(anyhow::anyhow!(
                "delete_observation: kind='{kind}' not allowed; only 'behavior' can be hard-deleted"
            ));
        }
        let id = args
            .get("id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| anyhow::anyhow!("delete_observation: missing 'id'"))?;
        let evidence = args
            .get("evidence")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow::anyhow!("delete_observation: missing 'evidence'"))?;
        if evidence.is_empty() {
            return Err(anyhow::anyhow!(
                "delete_observation: 'evidence' must contain at least one event id"
            ));
        }
        let n = ctx.memory.delete_behavior(id).await?;
        if let Err(e) = ctx
            .memory
            .log_event(
                &ctx.run_id,
                &ctx.agent_name,
                0,
                "observation_deleted",
                Some(serde_json::json!({
                    "kind": kind,
                    "id": id,
                    "evidence": evidence,
                    "rows_removed": n,
                })),
            )
            .await
        {
            tracing::warn!(
                kind = %kind,
                id = id,
                error = %e,
                "log_event failed for observation_deleted"
            );
        }
        Ok(serde_json::json!({ "deleted": true, "id": id, "rows_removed": n }))
    }
}