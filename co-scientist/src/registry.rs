//! Tool registry: a typed map from `name -> Tool`, with per-agent
//! allowlists and a JSON-schema validator that runs before `Tool::call`.
//!
//! This replaces the `[[MEMORY_OP:...]]` text-marker dispatch in
//! `runner.rs`. The runner still falls back to the marker parser for
//! backward compatibility, but new callers should use the registry.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use serde_json::Value;

use crate::tool::{Tool, ToolCtx, ToolOutput};

#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("names", &self.tools.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool. Overwrites any existing tool with the same name.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    /// Register many at once.
    pub fn register_all(&mut self, tools: impl IntoIterator<Item = Arc<dyn Tool>>) {
        for t in tools {
            self.register(t);
        }
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn names(&self) -> Vec<String> {
        let mut n: Vec<_> = self.tools.keys().cloned().collect();
        n.sort();
        n
    }

    /// Resolve a per-agent tool allowlist. Pass `Some(set)` to enable
    /// the named tools; pass `None` to fail-closed (returns empty).
    /// The agent name is currently informational; the allowlist alone
    /// determines which tools are returned.
    pub fn for_agent(&self, _agent: &str, allow: Option<&[&str]>) -> Vec<Arc<dyn Tool>> {
        let allow = match allow {
            Some(a) => a,
            None => return Vec::new(),
        };
        allow
            .iter()
            .filter_map(|name| self.tools.get(*name).cloned())
            .collect()
    }

    /// Dispatch a tool call. Validates the args against the tool's
    /// declared schema (basic type checks — no real JSONSchema
    /// validator to keep deps light) before invoking.
    ///
    /// Community tool names (`record_system_feedback`,
    /// `record_research_plan`) are rewritten to their local names
    /// (`save_behavior`, `save_semantic`) via
    /// [`crate::marker_normalizer::canonicalize`]. `record_hypothesis`
    /// and `record_review` are first-class tools and pass through.
    /// The original name is preserved in the error message so a
    /// failed dispatch is debuggable.
    pub async fn dispatch(
        &self,
        name: &str,
        args: Value,
        ctx: &ToolCtx,
    ) -> Result<ToolOutput> {
        let local = crate::marker_normalizer::canonicalize(name).unwrap_or(name);
        // `noop` and `none` are community-shared "I'm done" sentinels —
        // LLMs emit `[[MEMORY_OP:noop:{}]]` or `[[MEMORY_OP:none:{}]]`
        // to signal end of turn without doing anything. Treat both as
        // no-op successes rather than tool errors.
        if local == "noop" || local == "none" {
            return Ok(serde_json::json!({ "noop": true }));
        }
        let tool = self
            .get(local)
            .ok_or_else(|| anyhow!("unknown tool: {name}"))?;
        validate_args(&args, &tool.input_schema())?;
        tool.call(args, ctx).await
    }
}

/// Lightweight argument validator. Checks that:
///   - `args` is an object
///   - every `required` property is present
///   - every non-required property that is present has a matching type
///
/// This is not a full JSONSchema validator. It catches the common
/// programmer-error cases (missing required, wrong type) without pulling
/// in a 500KB crate.
fn validate_args(args: &Value, schema: &Value) -> Result<()> {
    let obj = args
        .as_object()
        .ok_or_else(|| anyhow!("args must be an object, got {}", args))?;
    let required: Vec<&str> = schema
        .get("required")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|s| s.as_str()).collect())
        .unwrap_or_default();
    for key in required {
        if !obj.contains_key(key) {
            return Err(anyhow!("missing required argument: {key}"));
        }
    }
    if let Some(props) = schema.get("properties").and_then(|v| v.as_object()) {
        for (k, v) in obj {
            if let Some(prop_schema) = props.get(k) {
                check_type(k, v, prop_schema)?;
            }
        }
    }
    Ok(())
}

fn check_type(key: &str, value: &Value, schema: &Value) -> Result<()> {
    let expected = schema.get("type").and_then(|v| v.as_str());
    let actual_type = match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    };
    if let Some(exp) = expected {
        if exp != actual_type && !(exp == "integer" && actual_type == "number") {
            return Err(anyhow!(
                "argument '{key}' has type '{actual_type}', expected '{exp}'"
            ));
        }
    }
    Ok(())
}

/// Default per-agent allowlist for the 6 co-scientist roles. The
/// community's tool names are accepted on top of our local names; the
/// registry rewrites them via [`crate::marker_normalizer::canonicalize`].
pub fn default_allowlist(agent: &str) -> Option<Vec<&'static str>> {
    let research = vec![
        "save_semantic",
        "save_behavior",
        "get_context",
        "compress_events",
        // 3-layer retrieval (peek → timeline → get_observation)
        "peek_context",
        "get_timeline",
        "get_observation",
        // structured research tools
        "record_hypothesis",
        "record_review",
        "record_tournament_match",
        // community aliases — accepted as-is, rewritten by the runner
        "record_system_feedback",
        "record_research_plan",
    ];
    let archive_tools = vec!["archive_observation", "delete_observation"];
    let ranking_only = vec!["record_tournament_match"];
    let experiment_tools = vec![
        // Empirical loop. `save_semantic` + `record_review` let the
        // experiment agent record its designs/results and contribute
        // back to the tournament.
        "save_semantic",
        "save_behavior",
        "get_context",
        "peek_context",
        "get_timeline",
        "get_observation",
        "design_experiment",
        "execute_experiment",
        "evaluate_result",
        "record_review",
    ];
    match agent {
        // Supervisor parses the goal into a research plan (calls
        // `record_research_plan`, which canonicalizes to
        // `save_semantic` with scope="plan") and curates its own
        // session (archive / delete junk observations). The prompt
        // ↔ allowlist validator enforces that both sides agree.
        "supervisor" => {
            let mut v = vec!["save_semantic", "record_research_plan"];
            v.extend(archive_tools.iter().copied());
            Some(v)
        }
        // Ranking only needs the tournament match tool.
        "ranking" => Some(ranking_only),
        // Research agents get the full set; metareview can also curate.
        "generation" | "reflection" | "evolution" => Some(research),
        "metareview" => {
            let mut v = research;
            v.extend(archive_tools.iter().copied());
            Some(v)
        }
        // Experiment agent — the empirical loop.
        "experiment" => Some(experiment_tools),
        // Legacy / pre-refactor names.
        "literature" | "hypothesis" | "analysis" | "critic"
        | "synthesizer" => Some(research),
        _ => Some(research),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::memory::Memory;
    use crate::tool::{builtin_tools, SaveSemanticTool, ToolCtx};
    use async_trait::async_trait;

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> String {
            "echoes the input".into()
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "msg": { "type": "string" }
                },
                "required": ["msg"]
            })
        }
        async fn call(&self, args: Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
            Ok(args)
        }
    }

    async fn make_ctx() -> ToolCtx {
        let mem = Memory::new(db::open_memory().await.unwrap());
        ToolCtx {
            memory: mem,
            run_id: "r".into(),
            agent_name: "a".into(),
        }
    }

    #[tokio::test]
    async fn dispatch_calls_registered_tool() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EchoTool));
        let ctx = make_ctx().await;
        let out = reg
            .dispatch("echo", serde_json::json!({"msg": "hi"}), &ctx)
            .await
            .unwrap();
        assert_eq!(out["msg"], "hi");
    }

    #[tokio::test]
    async fn dispatch_rejects_missing_required() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EchoTool));
        let ctx = make_ctx().await;
        let err = reg
            .dispatch("echo", serde_json::json!({}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("missing required"));
    }

    #[tokio::test]
    async fn dispatch_rejects_wrong_type() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EchoTool));
        let ctx = make_ctx().await;
        let err = reg
            .dispatch("echo", serde_json::json!({"msg": 42}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("type 'number'"));
    }

    #[tokio::test]
    async fn for_agent_filters_by_allowlist() {
        let mut reg = ToolRegistry::new();
        reg.register_all(builtin_tools());
        reg.register(Arc::new(EchoTool));
        let allow = default_allowlist("hypothesis").unwrap();
        let allowed = reg.for_agent("hypothesis", Some(&allow));
        // 7 memory/retrieval tools + 3 structured research tools = 10
        assert_eq!(allowed.len(), 10, "all memory + retrieval + research tools allowed");
        assert!(allowed.iter().any(|t| t.name() == "save_semantic"));
        assert!(allowed.iter().any(|t| t.name() == "record_hypothesis"));
    }

    #[tokio::test]
    async fn for_agent_with_no_allowlist_returns_empty() {
        let mut reg = ToolRegistry::new();
        reg.register_all(builtin_tools());
        let allowed = reg.for_agent("hypothesis", None);
        assert!(allowed.is_empty());
    }

    #[tokio::test]
    async fn save_semantic_via_registry_inserts_row() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(SaveSemanticTool));
        let ctx = make_ctx().await;
        let out = reg
            .dispatch(
                "save_semantic",
                serde_json::json!({"scope": "experiment", "summary": "via registry"}),
                &ctx,
            )
            .await
            .unwrap();
        let id = out["id"].as_i64().unwrap();
        assert!(id > 0);
    }
}
