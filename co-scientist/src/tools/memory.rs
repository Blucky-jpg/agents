//! Memory and retrieval tools: the day-to-day read/write surface an agent
//! uses to record observations and pull relevant context.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::memory::{ContextLimits, ObservationKind};

use super::{Tool, ToolCtx, ToolOutput};

// =====================================================================
// Built-in tools. Each closes over a `Memory` handle (set via the
// constructor) and exposes a thin argument schema for the LLM.
// =====================================================================

/// Tool: save a semantic memory. Schema mirrors `Memory::save_semantic`.
pub struct SaveSemanticTool;

#[async_trait]
impl Tool for SaveSemanticTool {
    fn name(&self) -> &str {
        "save_semantic"
    }
    fn description(&self) -> String {
        "Save an experiment, insight, result, or question to long-term memory. \
         Returns the new memory id."
            .to_string()
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "scope": {
                    "type": "string",
                    "enum": ["experiment", "insight", "result", "question"],
                    "description": "What kind of memory this is."
                },
                "summary": {
                    "type": "string",
                    "description": "One-sentence summary. The next agent scans this in 5 seconds."
                },
                "details": {
                    "description": "Arbitrary structured data (numbers, strings, lists, nested objects)."
                }
            },
            "required": ["scope", "summary"]
        })
    }
    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let scope = args
            .get("scope")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("save_semantic: missing 'scope'"))?;
        let summary = args
            .get("summary")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("save_semantic: missing 'summary'"))?;
        let details = args.get("details").cloned();
        let id = ctx
            .memory
            .save_semantic(&ctx.run_id, Some(&ctx.agent_name), scope, summary, details)
            .await?;
        Ok(serde_json::json!({ "id": id }))
    }
}

/// Tool: save a behavior pattern.
pub struct SaveBehaviorTool;

#[async_trait]
impl Tool for SaveBehaviorTool {
    fn name(&self) -> &str {
        "save_behavior"
    }
    fn description(&self) -> String {
        "Save a self-reflection / behavior pattern. The next time you spawn, \
         you will see this as part of your prior self-critique."
            .to_string()
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Short name for the pattern (e.g. 'concise-first-sentence')."
                },
                "notes": {
                    "type": "string",
                    "description": "Free-form observation. Used when error_summary/cause/fix are absent."
                },
                "error_summary": {
                    "type": "string",
                    "description": "What went wrong in one sentence. Preferred over notes when present."
                },
                "cause": {
                    "type": "string",
                    "description": "Why it went wrong in one sentence. (5-Whys step 1)"
                },
                "fix": {
                    "type": "string",
                    "description": "What to do differently in one sentence. (5-Whys step 2)"
                },
                "evidence": {
                    "description": "Optional list of event ids that triggered this observation.",
                    "type": "array",
                    "items": { "type": "integer" }
                }
            },
            "required": ["pattern"]
        })
    }
    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let pattern = args
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("save_behavior: missing 'pattern'"))?;
        // Prefer structured fields (error/cause/fix) when present; fall
        // back to free-form notes. This makes behavior memories usable
        // as retrieval hits, not just per-agent self-critique.
        let body = match (
            args.get("error_summary").and_then(|v| v.as_str()),
            args.get("cause").and_then(|v| v.as_str()),
            args.get("fix").and_then(|v| v.as_str()),
        ) {
            (Some(e), Some(c), Some(f)) => {
                format!("{} | cause: {} | fix: {}", e, c, f)
            }
            (Some(e), Some(c), None) => format!("{} | cause: {}", e, c),
            (Some(e), None, Some(f)) => format!("{} | fix: {}", e, f),
            (Some(e), None, None) => e.to_string(),
            _ => args
                .get("notes")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "save_behavior: provide 'notes' or at least 'error_summary'"
                    )
                })?
                .to_string(),
        };
        let evidence = args.get("evidence").cloned();
        let id = ctx
            .memory
            .save_behavior(&ctx.agent_name, pattern, &body, evidence)
            .await?;
        Ok(serde_json::json!({ "id": id }))
    }
}

/// Tool: fetch context (rendered for injection into the next user msg).
pub struct GetContextTool;

#[async_trait]
impl Tool for GetContextTool {
    fn name(&self) -> &str {
        "get_context"
    }
    fn description(&self) -> String {
        "Fetch the most relevant prior context for a question. Returns a \
         rendered markdown block ready to be put in front of a user message."
            .to_string()
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The question you're about to work on."
                },
                "events": { "type": "integer", "minimum": 0, "default": 5 },
                "semantic": { "type": "integer", "minimum": 0, "default": 5 },
                "behavior": { "type": "integer", "minimum": 0, "default": 3 },
                "max_tokens": { "type": "integer", "minimum": 0, "default": 0, "description": "Approximate token budget for rendered context. 0 = unlimited." },
                "full_count": { "type": "integer", "minimum": 0, "default": 3, "description": "How many recent memories get full detail. Rest shown as compact one-liners." }
            },
            "required": ["query"]
        })
    }
    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("get_context: missing 'query'"))?;
        let limits = ContextLimits {
            events: args.get("events").and_then(|v| v.as_u64()).unwrap_or(5) as usize,
            semantic: args.get("semantic").and_then(|v| v.as_u64()).unwrap_or(5) as usize,
            behavior: args.get("behavior").and_then(|v| v.as_u64()).unwrap_or(3) as usize,
            max_tokens: args.get("max_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
            full_count: args.get("full_count").and_then(|v| v.as_u64()).unwrap_or(3) as usize,
        };
        let ctx_block = ctx
            .memory
            .get_context(&ctx.run_id, &ctx.agent_name, query, limits)
            .await?;
        Ok(serde_json::json!({
            "rendered": ctx_block.rendered,
            "n_events": ctx_block.recent_events.len(),
            "n_semantic": ctx_block.semantic.len(),
            "n_behavior": ctx_block.behavior.len(),
        }))
    }
}

/// Tool: AI compression. Reads the last N events for the current
/// run and saves a model-generated summary as a `semantic_memories`
/// row with `scope = "compression"`. The model is expected to use
/// [`GetContextTool`] (or [`crate::memory::Memory::get_timeline`])
/// to read the events, then call this tool with its summary text.
///
/// We deliberately do NOT call the LLM inside this tool. The model
/// is the compressor; this tool is just the durable sink.
pub struct CompressEventsTool;

#[async_trait]
impl Tool for CompressEventsTool {
    fn name(&self) -> &str {
        "compress_events"
    }
    fn description(&self) -> String {
        "Save a model-generated summary of recent events for the current run. \
         The model is expected to have read the events (via get_context or \
         get_observation) and produced the summary itself. Use scope='compression' \
         for periodic rollups."
            .to_string()
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "summary": {
                    "type": "string",
                    "description": "Model-generated summary of the recent events. \
                                   One paragraph, durable insight, no preamble."
                },
                "scope": {
                    "type": "string",
                    "enum": ["compression", "session", "experiment"],
                    "default": "compression"
                },
                "details": {
                    "description": "Optional structured detail (key facts, ids, numbers)."
                }
            },
            "required": ["summary"]
        })
    }
    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let summary = args
            .get("summary")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("compress_events: missing 'summary'"))?;
        let scope = args
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("compression");
        let details = args.get("details").cloned();
        let id = ctx
            .memory
            .save_semantic(&ctx.run_id, Some(&ctx.agent_name), scope, summary, details)
            .await?;
        Ok(serde_json::json!({ "id": id, "scope": scope }))
    }
}

/// Tool: peek at relevant memories (compact one-liner scan).
/// Layer 1 of the 3-layer retrieval pattern. Returns id + summary
/// for the LLM to scan cheaply before fetching full detail.
pub struct PeekContextTool;

#[async_trait]
impl Tool for PeekContextTool {
    fn name(&self) -> &str {
        "peek_context"
    }
    fn description(&self) -> String {
        "Scan relevant memories by keyword. Returns compact one-liners (id + summary). \
         Use this FIRST to find relevant IDs, then call get_observation for full detail. \
         ~10x cheaper than get_context."
            .to_string()
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Keywords to search for (e.g. 'KRAS mutation resistance')."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 20,
                    "default": 10,
                    "description": "Max results to return."
                }
            },
            "required": ["query"]
        })
    }
    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("peek_context: missing 'query'"))?;
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(10) as usize;
        let results = ctx
            .memory
            .peek_context(&ctx.agent_name, query, limit)
            .await?;
        Ok(serde_json::json!({
            "results": results,
            "hint": "Call get_observation with kind + id for full detail."
        }))
    }
}

/// Tool: get events around a specific observation.
/// Layer 2 of the 3-layer retrieval pattern. Returns the chronological
/// context (events) surrounding a memory the LLM found relevant.
pub struct GetTimelineTool;

#[async_trait]
impl Tool for GetTimelineTool {
    fn name(&self) -> &str {
        "get_timeline"
    }
    fn description(&self) -> String {
        "Get events around a specific memory. Shows what happened before/after \
         an observation was saved. Use after peek_context to understand context."
            .to_string()
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "observation_id": {
                    "type": "integer",
                    "description": "The memory id from peek_context results."
                },
                "kind": {
                    "type": "string",
                    "enum": ["semantic", "behavior"],
                    "description": "Type of memory: 'semantic' for insights/hypotheses, 'behavior' for self-critique."
                },
                "around": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 10,
                    "default": 3,
                    "description": "Number of events before/after to include."
                }
            },
            "required": ["observation_id", "kind"]
        })
    }
    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let observation_id = args
            .get("observation_id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| anyhow::anyhow!("get_timeline: missing 'observation_id'"))?;
        let kind_str = args
            .get("kind")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("get_timeline: missing 'kind'"))?;
        let kind = match kind_str {
            "semantic" => ObservationKind::Semantic,
            "behavior" => ObservationKind::Behavior,
            _ => return Err(anyhow::anyhow!("get_timeline: kind must be 'semantic' or 'behavior'")),
        };
        let around = args
            .get("around")
            .and_then(|v| v.as_u64())
            .unwrap_or(3) as usize;
        let events = ctx
            .memory
            .get_timeline(observation_id, kind, around)
            .await?;
        Ok(serde_json::json!({
            "events": events,
            "count": events.len()
        }))
    }
}

/// Tool: get full detail for a single observation.
/// Layer 3 of the 3-layer retrieval pattern. Returns the complete row
/// (summary, details, evidence, etc.) for one memory the LLM selected.
pub struct GetObservationTool;

#[async_trait]
impl Tool for GetObservationTool {
    fn name(&self) -> &str {
        "get_observation"
    }
    fn description(&self) -> String {
        "Fetch full detail for a single memory by id. Use after peek_context \
         to get the complete row (summary, details, evidence, etc.)."
            .to_string()
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["semantic", "behavior"],
                    "description": "Type of memory."
                },
                "id": {
                    "type": "integer",
                    "description": "The memory id from peek_context results."
                }
            },
            "required": ["kind", "id"]
        })
    }
    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let id = args
            .get("id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| anyhow::anyhow!("get_observation: missing 'id'"))?;
        let kind_str = args
            .get("kind")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("get_observation: missing 'kind'"))?;
        let kind = match kind_str {
            "semantic" => ObservationKind::Semantic,
            "behavior" => ObservationKind::Behavior,
            _ => return Err(anyhow::anyhow!("get_observation: kind must be 'semantic' or 'behavior'")),
        };
        let obs = ctx
            .memory
            .get_observation(kind, id)
            .await?;
        match obs {
            Some(o) => Ok(serde_json::json!({ "observation": o })),
            None => Ok(serde_json::json!({ "observation": null, "error": "not found or archived" })),
        }
    }
}