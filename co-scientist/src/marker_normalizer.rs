//! Normalize a marker (`[[MEMORY_OP:<op>:{...}]]`) into the canonical
//! `(canonical_op, payload)` pair the [`ToolRegistry`] expects.
//!
//! ## Why this exists
//!
//! The community's prompt wording asks the model to call tools named
//! `record_hypothesis`, `record_review`, `record_system_feedback`, and
//! `record_research_plan`. The runtime registry uses a smaller, internally
//! consistent set (`save_semantic`, `save_behavior`, etc.). Two of the
//! community names are now first-class tools and pass through unchanged;
//! the other two are aliases onto existing tools, with prompt-convention
//! defaults baked in (e.g. `record_research_plan` implies `scope="plan"`).
//!
//! The alias table plus scope/summary inference previously lived inside
//! `runner::dispatch_marker`. That concentrated three unrelated concerns in
//! the orchestrator and made the heuristics untestable without a full
//! `Memory` fixture. Pulling them here turns the work into a pure function
//! with a tight, unit-testable contract.
//!
//! ## Design contract
//!
//! - **Pure**: no DB, no `Memory`, no async. The function takes a marker,
//!   returns either a normalized pair or an error.
//! - **Aliases are data, not code**: adding a community alias means adding
//!   one row to [`ALIASES`]. No new match arm.
//! - **First-class tools pass through**: `record_hypothesis` and
//!   `record_review` are not in the table; their canonical name is
//!   themselves.
//! - **Validation lives at the seam**: when the canonical op is
//!   `save_semantic` and the payload is missing `summary` with no
//!   recognized fallback (`objective` / `verdict` / `statement`), the
//!   normalizer rejects. The error propagates to `dispatch_marker`, which
//!   publishes `MemoryEvent::MarkerFailed` and logs `memory_op_failed`.
//!   The runner's `recent_marker_errors` self-correction loop then
//!   surfaces the error in the next turn's system prompt.
//!
//! ## What this module does NOT do
//!
//! - It does not look up the canonical tool in the registry. The registry
//!   does that.
//! - It does not call `Memory`. Pure function only.
//! - It does not parse markers. [`crate::skill::parse_markers`] does that.

use anyhow::{anyhow, Result};
use serde_json::Value;

/// One row of the alias table. `from` is the community prompt name; `to`
/// is the canonical name in the [`ToolRegistry`]. `implied_scope` is the
/// `scope` value auto-filled into a `save_semantic` payload when the
/// caller used the alias but forgot the field.
#[derive(Debug, Clone, Copy)]
pub struct ToolAlias {
    pub from: &'static str,
    pub to: &'static str,
    pub implied_scope: Option<&'static str>,
}

/// The community-prompt alias table.
///
/// Two entries:
/// - `record_system_feedback` → `save_behavior` (no scope — the resulting
///   `save_behavior` payload doesn't carry a scope).
/// - `record_research_plan`   → `save_semantic` with `scope="plan"`.
///
/// `record_hypothesis` and `record_review` are first-class tools (see
/// `tool.rs::RecordHypothesisTool`, `RecordReviewTool`) and pass through
/// unchanged — they are deliberately not in this table.
const ALIASES: &[ToolAlias] = &[
    ToolAlias {
        from: "record_system_feedback",
        to: "save_behavior",
        implied_scope: None,
    },
    ToolAlias {
        from: "record_research_plan",
        to: "save_semantic",
        implied_scope: Some("plan"),
    },
];

/// Look up the canonical name for a raw marker op.
///
/// Returns `Some(canonical)` if `raw` is in the alias table, `None` if
/// `raw` is already canonical (e.g. `record_hypothesis`, `save_semantic`,
/// or any registered tool the model called directly).
pub fn canonicalize(raw: &str) -> Option<&'static str> {
    ALIASES
        .iter()
        .find(|a| a.from == raw)
        .map(|a| a.to)
}

/// Normalize a marker into `(canonical_op, payload)`.
///
/// Steps, in order:
/// 1. If `raw_op` is in the alias table, use `to` as the canonical op.
///    Otherwise `raw_op` is already canonical.
/// 2. If the canonical op is `save_semantic`:
///    - Fill `scope` from the alias's `implied_scope` when `scope` is absent.
///    - Derive `summary` from `objective` / `verdict` / `statement` when absent; trim to ≤200 chars.
///    - If `summary` is still missing, reject with the IMPROVEMENT_PLAN §1.1 error.
/// 3. For all other canonical ops, the payload passes through unchanged.
///
/// The original `raw_op` is not preserved in the return value. Callers
/// that need the original (for logging / `MarkerFailed` events) must
/// keep it themselves.
pub fn normalize(raw_op: &str, payload: Value) -> Result<(String, Value)> {
    // Look up the alias for `raw_op`. `Some((canonical, implied_scope))`
    // when the caller used a community alias; `None` when `raw_op` is
    // already canonical (a first-class tool or `save_semantic` direct).
    let (canonical, implied_scope) = match ALIASES.iter().find(|a| a.from == raw_op) {
        Some(a) => (a.to.to_string(), a.implied_scope),
        None => (raw_op.to_string(), None),
    };

    if canonical != "save_semantic" {
        return Ok((canonical, payload));
    }

    let mut payload = payload;
    let obj = payload
        .as_object_mut()
        .ok_or_else(|| anyhow!("save_semantic: payload must be a JSON object"))?;

    // Step 2a: scope inference from the alias's implied_scope. Only
    // applies when the caller used the alias — a direct `save_semantic`
    // caller is responsible for its own scope, and the tool's own
    // validation will reject a missing one.
    if obj.get("scope").is_none()
        && let Some(scope) = implied_scope
    {
        obj.insert("scope".into(), Value::String(scope.into()));
    }

    // Step 2b: derive summary from common alternatives if missing.
    if obj.get("summary").is_none() {
        if let Some(derived) = obj
            .get("objective")
            .and_then(|v| v.as_str())
            .or_else(|| obj.get("verdict").and_then(|v| v.as_str()))
            .or_else(|| obj.get("statement").and_then(|v| v.as_str()))
            .map(derive_summary)
        {
            obj.insert("summary".into(), Value::String(derived));
        } else {
            return Err(anyhow!(
                "save_semantic: missing 'summary' and no recognized \
                 alternative (objective/verdict/statement)"
            ));
        }
    }

    Ok((canonical, payload))
}

/// Trim a summary candidate to ≤200 chars, appending an ellipsis when
/// truncated. Public to allow testing the truncation rule in isolation.
pub fn derive_summary(s: &str) -> String {
    let s = s.trim();
    if s.len() > 200 {
        format!("{}…", &s[..197])
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- canonicalize -------------------------------------------------------

    #[test]
    fn canonicalize_first_class_tools_pass_through_as_none() {
        // First-class tools are not aliases — canonicalize returns None so
        // the caller treats `raw` itself as canonical.
        assert_eq!(canonicalize("record_hypothesis"), None);
        assert_eq!(canonicalize("record_review"), None);
        assert_eq!(canonicalize("save_semantic"), None);
        assert_eq!(canonicalize("save_behavior"), None);
    }

    #[test]
    fn canonicalize_known_aliases() {
        assert_eq!(canonicalize("record_system_feedback"), Some("save_behavior"));
        assert_eq!(canonicalize("record_research_plan"), Some("save_semantic"));
    }

    // ---- normalize: pass-through cases --------------------------------------

    #[test]
    fn normalize_passes_through_first_class_record_hypothesis() {
        let (op, payload) = normalize("record_hypothesis", json!({"summary": "x"})).unwrap();
        assert_eq!(op, "record_hypothesis");
        assert_eq!(payload, json!({"summary": "x"}));
    }

    #[test]
    fn normalize_passes_through_save_behavior_unchanged() {
        let (op, payload) = normalize(
            "save_behavior",
            json!({"pattern": "p", "notes": "n"}),
        )
        .unwrap();
        assert_eq!(op, "save_behavior");
        assert_eq!(payload, json!({"pattern": "p", "notes": "n"}));
    }

    #[test]
    fn normalize_passes_through_save_semantic_when_scope_and_summary_present() {
        let (op, payload) = normalize(
            "save_semantic",
            json!({"scope": "experiment", "summary": "x"}),
        )
        .unwrap();
        assert_eq!(op, "save_semantic");
        assert_eq!(payload, json!({"scope": "experiment", "summary": "x"}));
    }

    #[test]
    fn normalize_preserves_explicit_scope_over_implied() {
        // Even when the alias implies scope=plan, an explicit scope wins.
        let (op, payload) = normalize(
            "record_research_plan",
            json!({"scope": "experiment", "summary": "x"}),
        )
        .unwrap();
        assert_eq!(op, "save_semantic");
        assert_eq!(payload["scope"], "experiment");
    }

    // ---- normalize: scope inference -----------------------------------------

    #[test]
    fn normalize_alias_record_research_plan_fills_scope_plan() {
        let (op, payload) = normalize(
            "record_research_plan",
            json!({"summary": "research plan summary"}),
        )
        .unwrap();
        assert_eq!(op, "save_semantic");
        assert_eq!(payload["scope"], "plan");
        assert_eq!(payload["summary"], "research plan summary");
    }

    #[test]
    fn normalize_save_semantic_without_alias_does_not_inject_scope() {
        // Direct save_semantic with missing scope must NOT be filled — the
        // tool's own validation rejects it. The normalizer only fills
        // scope when the caller used a scope-implying alias.
        let (_, payload) = normalize("save_semantic", json!({"summary": "x"})).unwrap();
        assert!(payload.get("scope").is_none());
    }

    // ---- normalize: summary derivation --------------------------------------

    #[test]
    fn normalize_derives_summary_from_objective() {
        let (_, payload) = normalize(
            "save_semantic",
            json!({"scope": "experiment", "objective": "do the thing"}),
        )
        .unwrap();
        assert_eq!(payload["summary"], "do the thing");
    }

    #[test]
    fn normalize_derives_summary_from_verdict() {
        let (_, payload) = normalize(
            "save_semantic",
            json!({"scope": "review", "verdict": "looks good"}),
        )
        .unwrap();
        assert_eq!(payload["summary"], "looks good");
    }

    #[test]
    fn normalize_derives_summary_from_statement() {
        let (_, payload) = normalize(
            "save_semantic",
            json!({"scope": "hypothesis", "statement": "x predicts y"}),
        )
        .unwrap();
        assert_eq!(payload["summary"], "x predicts y");
    }

    #[test]
    fn normalize_summary_fallback_precedence_is_objective_then_verdict_then_statement() {
        // objective wins when all three are present.
        let (_, payload) = normalize(
            "save_semantic",
            json!({
                "scope": "s",
                "objective": "from-objective",
                "verdict": "from-verdict",
                "statement": "from-statement",
            }),
        )
        .unwrap();
        assert_eq!(payload["summary"], "from-objective");

        // verdict wins when objective is absent.
        let (_, payload) = normalize(
            "save_semantic",
            json!({
                "scope": "s",
                "verdict": "from-verdict",
                "statement": "from-statement",
            }),
        )
        .unwrap();
        assert_eq!(payload["summary"], "from-verdict");

        // statement is the final fallback.
        let (_, payload) = normalize(
            "save_semantic",
            json!({"scope": "s", "statement": "from-statement"}),
        )
        .unwrap();
        assert_eq!(payload["summary"], "from-statement");
    }

    #[test]
    fn normalize_derives_summary_with_truncation() {
        let long = "a".repeat(500);
        let (_, payload) = normalize(
            "save_semantic",
            json!({"scope": "s", "objective": long}),
        )
        .unwrap();
        let s = payload["summary"].as_str().unwrap();
        assert!(s.ends_with('…'));
        assert!(s.chars().count() <= 200);
    }

    #[test]
    fn normalize_trims_whitespace_in_derived_summary() {
        let (_, payload) = normalize(
            "save_semantic",
            json!({"scope": "s", "objective": "  hello  "}),
        )
        .unwrap();
        assert_eq!(payload["summary"], "hello");
    }

    // ---- normalize: rejection (§1.1 fix) ------------------------------------

    #[test]
    fn normalize_rejects_save_semantic_missing_summary() {
        // The IMPROVEMENT_PLAN §1.1 fix: this is the error that lets the
        // self-correction loop fire instead of writing a junk row.
        let err = normalize(
            "save_semantic",
            json!({"scope": "experiment"}),
        )
        .unwrap_err();
        assert!(err.to_string().contains("missing 'summary'"));
    }

    #[test]
    fn normalize_rejects_empty_object_payload_for_save_semantic() {
        let err = normalize("save_semantic", json!({})).unwrap_err();
        assert!(err.to_string().contains("missing 'summary'"));
    }

    #[test]
    fn normalize_rejects_non_object_payload_for_save_semantic() {
        let err = normalize("save_semantic", json!("just a string")).unwrap_err();
        assert!(err.to_string().contains("payload must be a JSON object"));
    }

    // ---- normalize: alias interaction ---------------------------------------

    #[test]
    fn normalize_record_research_plan_with_objective_derives_summary() {
        // Combined: alias fills scope, objective fills summary.
        let (op, payload) = normalize(
            "record_research_plan",
            json!({"objective": "do x then y"}),
        )
        .unwrap();
        assert_eq!(op, "save_semantic");
        assert_eq!(payload["scope"], "plan");
        assert_eq!(payload["summary"], "do x then y");
    }

    #[test]
    fn normalize_record_system_feedback_passes_through_to_save_behavior() {
        // record_system_feedback has no implied scope, so the payload
        // passes through unchanged (save_behavior's own validation
        // rejects missing 'pattern' downstream).
        let (op, payload) = normalize(
            "record_system_feedback",
            json!({"pattern": "p", "notes": "n"}),
        )
        .unwrap();
        assert_eq!(op, "save_behavior");
        assert_eq!(payload, json!({"pattern": "p", "notes": "n"}));
    }

    // ---- derive_summary -----------------------------------------------------

    #[test]
    fn derive_summary_short_passes_through_trimmed() {
        assert_eq!(derive_summary("  hello  "), "hello");
    }

    #[test]
    fn derive_summary_long_truncates_with_ellipsis() {
        let s = "x".repeat(300);
        let out = derive_summary(&s);
        assert!(out.ends_with('…'));
        assert!(out.chars().count() <= 200);
    }
}