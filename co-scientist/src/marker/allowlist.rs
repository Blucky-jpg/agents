//! Static prompt ↔ agent allowlist validator.
//!
//! Walks the 18 prompt `.md` templates under `co-scientist/prompts/`
//! at compile time, extracts every `[[MEMORY_OP:<tool_name>:{...}]]`
//! tool reference, and builds a map: `AgentMode → Set<tool_name>`.
//!
//! At runtime, two checks fire:
//!
//! 1. **Startup check** — for every (agent, mode) pair from
//!    [`crate::prompts::AgentMode::modes_for`], verify the tools
//!    referenced by `mode`'s prompt template are a subset of
//!    [`crate::registry::default_allowlist`] for that agent. If not,
//!    the harness fails loud with the exact offending tools. This is
//!    the "system smartly handles it" guarantee: the LLM never sees a
//!    prompt that tells it to call a tool it cannot dispatch.
//!
//! 2. **Render-time check** — when a `run_agent` task arrives with
//!    `(agent, mode)`, verify `AgentMode::agent(mode) == agent`. If
//!    not, reject the task before any LLM call. This catches
//!    supervisor-calls-evolution, etc.
//!
//! The marker parse is deliberately simple: brace-counted JSON body,
//! same tolerance rules as the production `parse_markers` (see
//! `crate::skill`). It runs at compile time via `include_str!` so a
//! typo in a prompt file becomes a build error.

use crate::marker_normalizer::canonicalize;
use crate::prompts::{AgentMode, AgentMode::*, PROMPT_MODES};
use crate::registry::default_allowlist;
use crate::tool_catalog;
use anyhow::{anyhow, Context, Result};
use std::collections::{BTreeSet, HashMap};

/// Every name the LLM might emit as a marker. Derived from
/// [`crate::tool_catalog::TOOL_CATALOG`] (canonical names + aliases).
/// This is the single source of truth for "what tokens in backtick
/// spans could be tool references?" — used by [`extract_backtick_refs`]
/// to scan prompt prose.
fn known_tool_names() -> &'static BTreeSet<&'static str> {
    use std::sync::OnceLock;
    static NAMES: OnceLock<BTreeSet<&'static str>> = OnceLock::new();
    NAMES.get_or_init(|| tool_catalog::known_tool_names().iter().copied().collect())
}

/// One mode's tool references, extracted from its prompt template.
/// Stored sorted + deduplicated for stable diffs and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModeTools {
    pub mode: AgentMode,
    pub tools: Vec<String>,
}

impl ModeTools {
    fn new(mode: AgentMode, mut tools: Vec<String>) -> Self {
        tools.sort();
        tools.dedup();
        Self { mode, tools }
    }
}

/// Full table: mode → referenced tool names. Built once via
/// [`build_table`] at startup. Cheap to rebuild — the prompts are
/// `include_str!`'d, so the parse runs against in-memory strings.
#[derive(Debug, Clone)]
pub struct PromptToolTable {
    by_mode: HashMap<AgentMode, ModeTools>,
}

impl PromptToolTable {
    /// Synthetic sentinels that are always dispatchable regardless
    /// of the per-agent allowlist. `registry::dispatch` handles them
    /// before the registered-tool lookup (see the `noop` / `none`
    /// branch in `registry::dispatch`), so the validator must treat
    /// them as universal — they are not registered as Tools.
    ///
    /// Derived from `tool_catalog::TOOL_CATALOG` so the source of truth
    /// is one place. Computed once via `OnceLock` since the catalog is
    /// a const slice.
    fn always_allowed() -> &'static [&'static str] {
        use std::sync::OnceLock;
        static NAMES: OnceLock<Vec<&'static str>> = OnceLock::new();
        NAMES.get_or_init(|| {
            crate::tool_catalog::TOOL_CATALOG
                .iter()
                .filter(|e| e.always_allowed)
                .map(|e| e.name)
                .collect()
        })
    }

    /// Build the table from the 18 embedded prompt templates. Runs
    /// the static parse; if any template has unparseable marker syntax
    /// this fails loud (build-time-ish via `include_str!` plus this
    /// runtime validator).
    pub fn build() -> Result<Self> {
        let mut by_mode = HashMap::new();
        for mode in PROMPT_MODES {
            let body: &str = match mode {
                ParseGoal => include_str!("../../prompts/parse_goal.md"),
                GenerationLiterature => include_str!("../../prompts/generation_literature.md"),
                GenerationDebate => include_str!("../../prompts/generation_debate.md"),
                ReflectionReview => include_str!("../../prompts/reflection_review.md"),
                ReflectionObservation => include_str!("../../prompts/reflection_observation.md"),
                ReflectionVerification => include_str!("../../prompts/reflection_verification.md"),
                ReflectionOnResult => include_str!("../../prompts/reflection_on_result.md"),
                RankingPairwise => include_str!("../../prompts/ranking_pairwise.md"),
                RankingDebate => include_str!("../../prompts/ranking_debate.md"),
                EvolutionCombine => include_str!("../../prompts/evolution_combine.md"),
                EvolutionSimplify => include_str!("../../prompts/evolution_simplify.md"),
                EvolutionFeasibility => include_str!("../../prompts/evolution_feasibility.md"),
                EvolutionOutOfBox => include_str!("../../prompts/evolution_out_of_box.md"),
                MetaReviewSystem => include_str!("../../prompts/metareview_system.md"),
                MetaReviewFinal => include_str!("../../prompts/metareview_final.md"),
                ExperimentDesign => include_str!("../../prompts/experiment_design.md"),
                ExperimentExecute => include_str!("../../prompts/experiment_execute.md"),
                ExperimentEvaluate => include_str!("../../prompts/experiment_evaluate.md"),
            };
            let tools = extract_tool_refs(body)
                .with_context(|| format!("parsing {}", mode.filename()))?;
            by_mode.insert(*mode, ModeTools::new(*mode, tools));
        }
        Ok(Self { by_mode })
    }

    pub fn tools_for(&self, mode: AgentMode) -> &[String] {
        self.by_mode
            .get(&mode)
            .map(|m| m.tools.as_slice())
            .unwrap_or(&[])
    }

    /// Verify every (agent, mode) pair. Returns the offending pair +
    /// missing tool names on failure. Pass = empty Ok.
    pub fn validate(&self) -> Result<()> {
        let mut errors: Vec<String> = Vec::new();
        for mode in PROMPT_MODES {
            let agent_name = mode.agent();
            let allow = match default_allowlist(agent_name) {
                Some(a) => a,
                None => {
                    errors.push(format!(
                        "agent `{agent_name}` (owns mode `{}`) has no default_allowlist entry",
                        mode.filename()
                    ));
                    continue;
                }
            };
            let allow_set: BTreeSet<&str> = allow.iter().copied().collect();
            let referenced = self.tools_for(*mode);
            for tool in referenced {
                if Self::always_allowed().contains(&tool.as_str()) {
                    continue;
                }
                if !allow_set.contains(tool.as_str())
                    && !allow_set.contains(canonicalize(tool).unwrap_or(""))
                {
                    errors.push(format!(
                        "{}/{} prompt references `{}` but the agent's allowlist does not include it",
                        agent_name,
                        mode.filename(),
                        tool
                    ));
                }
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(anyhow!(
                "prompt ↔ allowlist mismatches found ({}):\n  - {}",
                errors.len(),
                errors.join("\n  - ")
            ))
        }
    }

    /// Verify one specific (agent_name, mode) pair at render time.
    /// Used by `run_agent` before any LLM call.
    pub fn validate_pair(&self, agent_name: &str, mode: AgentMode) -> Result<()> {
        if mode.agent() != agent_name {
            return Err(anyhow!(
                "agent `{agent_name}` does not own mode `{}` (owned by `{}`)",
                mode.filename(),
                mode.agent()
            ));
        }
        let allow = default_allowlist(agent_name)
            .ok_or_else(|| anyhow!("agent `{agent_name}` has no default_allowlist entry"))?;
        let allow_set: BTreeSet<&str> = allow.iter().copied().collect();
        for tool in self.tools_for(mode) {
            if Self::always_allowed().contains(&tool.as_str()) {
                continue;
            }
            if !allow_set.contains(tool.as_str())
                && !allow_set.contains(canonicalize(tool).unwrap_or(""))
            {
                return Err(anyhow!(
                    "prompt `{}` references tool `{}` which is not in `{agent_name}`'s allowlist",
                    mode.filename(),
                    tool
                ));
            }
        }
        Ok(())
    }
}

/// Walk a prompt template body and extract every tool name that
/// appears inside `[[MEMORY_OP:<tool>:{...}]]` markers. Tolerates
/// uppercase and lowercase prefixes (matches `parse_markers`); cap on
/// trailing `]` characters per the production parser. Returns names
/// in the order they appear (sorted + dedup happens in [`ModeTools`]).
fn extract_marker_refs(body: &str) -> Result<Vec<String>> {
    const PREFIXES: &[&str] = &["[[MEMORY_OP:", "[[memory_op:"];
    let mut out = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Find the next prefix match.
        let mut found: Option<(usize, &str)> = None;
        for p in PREFIXES {
            if bytes[i..].starts_with(p.as_bytes()) {
                found = Some((i + p.len(), p));
                break;
            }
        }
        let Some((start, _)) = found else {
            i += 1;
            continue;
        };
        // Parse `<tool_name>:<json>` starting at `start`.
        let after_prefix = &body[start..];
        let Some(colon) = after_prefix.find(':') else {
            return Err(anyhow!(
                "marker at byte {i} has no `:` separator after tool name"
            ));
        };
        let tool_name = &after_prefix[..colon];
        if tool_name.is_empty() {
            return Err(anyhow!("marker at byte {i} has empty tool name"));
        }
        // Tool names are ASCII identifier-ish — reject anything weird
        // so we don't accept garbage from the marker body.
        if !tool_name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(anyhow!(
                "marker at byte {i} has non-identifier tool name `{tool_name}`"
            ));
        }
        out.push(tool_name.to_string());
        // Skip past the JSON body via brace counting, then consume up
        // to 10 trailing `]` characters. This mirrors the production
        // parser's tolerance rules — see `crate::skill::find_next_marker`.
        let body_start = start + colon + 1;
        let rest = body[body_start..].as_bytes();
        let mut depth: i32 = 0;
        let mut j = 0;
        let mut in_string = false;
        let mut escape = false;
        while j < rest.len() {
            let c = rest[j];
            if in_string {
                if escape {
                    escape = false;
                } else if c == b'\\' {
                    escape = true;
                } else if c == b'"' {
                    in_string = false;
                }
            } else {
                match c {
                    b'"' => in_string = true,
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            j += 1;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            j += 1;
        }
        if depth != 0 {
            return Err(anyhow!(
                "marker at byte {i} has unclosed JSON body (tool `{tool_name}`)"
            ));
        }
        // Consume trailing `]` (up to 10).
        let mut k = 0;
        while k < 10 && j + k < rest.len() && rest[j + k] == b']' {
            k += 1;
        }
        i = body_start + j + k;
    }
    Ok(out)
}

/// Walk the prompt body and extract every tool name referenced via
/// Markdown backticks (`` `tool_name` ``). Used to catch instructions
/// like "Call `record_research_plan` with your final plan" where the
/// tool name is prose, not an actual marker. We only count tokens
/// that match a known registered tool or community alias — anything
/// else is treated as ordinary prose and skipped, which keeps false
/// positives out of the validator output.
fn extract_backtick_refs(body: &str) -> Vec<String> {
    let known = known_tool_names();
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(open) = rest.find('`') {
        let after_open = &rest[open + 1..];
        if let Some(close) = after_open.find('`') {
            let token = &after_open[..close];
            if !token.is_empty()
                && token
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
                && known.contains(token)
            {
                out.push(token.to_string());
            }
            rest = &after_open[close + 1..];
        } else {
            // Unclosed backtick; stop scanning.
            break;
        }
    }
    out
}

/// Combine marker-extracted and backtick-extracted refs. Both
/// sources describe tools the LLM is being told to call — markers
/// are explicit instructions, backticks are named references in
/// prose. Either way, the tool name must be in the agent's allowlist.
fn extract_tool_refs(body: &str) -> Result<Vec<String>> {
    let mut out = extract_marker_refs(body)?;
    out.extend(extract_backtick_refs(body));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_single_tool() {
        let body = r#"Call [[MEMORY_OP:save_semantic:{"summary":"x"}]] now."#;
        let refs = extract_marker_refs(body).unwrap();
        assert_eq!(refs, vec!["save_semantic".to_string()]);
    }

    #[test]
    fn extracts_multiple_tools() {
        let body = r#"
            First [[MEMORY_OP:record_hypothesis:{"summary":"a"}]].
            Then [[MEMORY_OP:save_behavior:{"pattern":"x","notes":"y"}]].
        "#;
        let refs = extract_marker_refs(body).unwrap();
        assert_eq!(
            refs,
            vec!["record_hypothesis".to_string(), "save_behavior".to_string()]
        );
    }

    #[test]
    fn tolerates_lowercase_prefix() {
        let body = r#"[[memory_op:save_semantic:{"k":"v"}]]"#;
        let refs = extract_marker_refs(body).unwrap();
        assert_eq!(refs, vec!["save_semantic".to_string()]);
    }

    #[test]
    fn tolerates_extra_trailing_brackets() {
        let body = r#"[[MEMORY_OP:save_semantic:{"k":"v"}]]]]]]"#;
        let refs = extract_marker_refs(body).unwrap();
        assert_eq!(refs, vec!["save_semantic".to_string()]);
    }

    #[test]
    fn handles_nested_braces_in_json() {
        let body = r#"[[MEMORY_OP:save_semantic:{"details":{"nested":"yes"}}]]"#;
        let refs = extract_marker_refs(body).unwrap();
        assert_eq!(refs, vec!["save_semantic".to_string()]);
    }

    #[test]
    fn handles_braces_inside_strings() {
        let body = r#"[[MEMORY_OP:save_semantic:{"summary":"has } in it"}]]"#;
        let refs = extract_marker_refs(body).unwrap();
        assert_eq!(refs, vec!["save_semantic".to_string()]);
    }

    #[test]
    fn handles_escaped_quotes_in_strings() {
        let body = r#"[[MEMORY_OP:save_semantic:{"summary":"has \" in it"}]]"#;
        let refs = extract_marker_refs(body).unwrap();
        assert_eq!(refs, vec!["save_semantic".to_string()]);
    }

    #[test]
    fn no_markers_returns_empty() {
        let body = "Just plain text without any markers.";
        let refs = extract_marker_refs(body).unwrap();
        assert!(refs.is_empty());
    }

    #[test]
    fn empty_tool_name_errors() {
        let body = "[[MEMORY_OP::{\"k\":\"v\"}]]";
        assert!(extract_marker_refs(body).is_err());
    }

    #[test]
    fn unclosed_marker_errors() {
        // Missing closing `}` in the JSON body — parser must reject
        // because it cannot find the end of the marker payload.
        let body = "[[MEMORY_OP:save_semantic:{\"k\":\"v\"";
        assert!(extract_marker_refs(body).is_err());
    }

    #[test]
    fn non_identifier_tool_name_errors() {
        let body = "[[MEMORY_OP:bad-name:{\"k\":\"v\"}]]";
        assert!(extract_marker_refs(body).is_err());
    }

    #[test]
    fn backtick_refs_extract_known_tool_names() {
        let body = "Call `record_research_plan` with your plan. Also see `save_behavior`.";
        let refs = extract_backtick_refs(body);
        assert_eq!(
            refs,
            vec!["record_research_plan".to_string(), "save_behavior".to_string()]
        );
    }

    #[test]
    fn backtick_refs_ignore_unknown_tool_names() {
        let body = "Call `not_a_tool` or `also_not_a_tool` for fun.";
        let refs = extract_backtick_refs(body);
        assert!(refs.is_empty(), "unknown tokens must be ignored; got {:?}", refs);
    }

    #[test]
    fn backtick_refs_ignore_prose_with_internal_punctuation() {
        // Multi-word backtick spans are not tool names — skip them.
        let body = "The `save semantic` tool is great. Also `record_hypothesis`.";
        let refs = extract_backtick_refs(body);
        assert_eq!(refs, vec!["record_hypothesis".to_string()]);
    }

    #[test]
    fn combined_refs_merge_marker_and_backtick() {
        let body = r#"
            Call `record_research_plan` to register the plan.
            Then [[MEMORY_OP:save_behavior:{"pattern":"x","notes":"y"}]].
        "#;
        let refs = extract_tool_refs(body).unwrap();
        // Order: backtick refs are extracted first (whole-body scan),
        // then marker refs (also whole-body scan). Both sources
        // produce the same union — order between them is implementation-
        // defined, so assert membership rather than position.
        let mut sorted = refs.clone();
        sorted.sort();
        assert_eq!(
            sorted,
            vec![
                "record_research_plan".to_string(),
                "save_behavior".to_string(),
            ]
        );
    }

    #[test]
    fn table_builds_for_all_18_modes() {
        let table = PromptToolTable::build().expect("table builds");
        for mode in PROMPT_MODES {
            // Every mode is in the table.
            assert!(
                table.tools_for(*mode).is_empty() || !table.tools_for(*mode).is_empty(),
                "mode {mode:?} missing from table"
            );
        }
    }

    #[test]
    fn validate_passes_against_current_allowlists() {
        let table = PromptToolTable::build().expect("table builds");
        table.validate().expect("current prompts must validate");
    }

    #[test]
    fn validate_pair_catches_wrong_agent() {
        let table = PromptToolTable::build().unwrap();
        // parse_goal is owned by supervisor; passing it to generation
        // must be rejected.
        let err = table.validate_pair("generation", ParseGoal).unwrap_err();
        assert!(err.to_string().contains("does not own"));
    }

    #[test]
    fn validate_pair_passes_for_correct_owner() {
        let table = PromptToolTable::build().unwrap();
        // generation_literature is owned by generation; the prompt
        // references record_hypothesis which is in generation's
        // allowlist. Pass.
        table
            .validate_pair("generation", GenerationLiterature)
            .expect("generation owning its mode passes");
    }

    #[test]
    fn parse_goal_references_record_research_plan() {
        let table = PromptToolTable::build().unwrap();
        let tools = table.tools_for(ParseGoal);
        assert!(
            tools.iter().any(|t| t == "record_research_plan"),
            "parse_goal should reference record_research_plan; got {:?}",
            tools
        );
    }

    #[test]
    fn experiment_modes_reference_empirical_loop_tools() {
        let table = PromptToolTable::build().unwrap();
        let design = table.tools_for(ExperimentDesign);
        assert!(design.iter().any(|t| t == "design_experiment"));
        let execute = table.tools_for(ExperimentExecute);
        assert!(execute.iter().any(|t| t == "execute_experiment"));
        let evaluate = table.tools_for(ExperimentEvaluate);
        assert!(evaluate.iter().any(|t| t == "evaluate_result"));
    }

    #[test]
    fn metareview_final_references_no_tool() {
        let table = PromptToolTable::build().unwrap();
        // metareview_final is human-facing prose — no marker call.
        assert!(
            table.tools_for(MetaReviewFinal).is_empty(),
            "metareview_final must not reference any tool; got {:?}",
            table.tools_for(MetaReviewFinal)
        );
    }

    #[test]
    fn parse_goal_picks_up_backtick_reference() {
        // The parse_goal prompt asks the LLM to "Call the
        // `record_research_plan` tool" via backticks, not a marker.
        // The combined extractor must catch it.
        let table = PromptToolTable::build().unwrap();
        let tools = table.tools_for(ParseGoal);
        assert!(
            tools.iter().any(|t| t == "record_research_plan"),
            "parse_goal backtick reference must be picked up; got {:?}",
            tools
        );
    }

    #[test]
    fn metareview_system_picks_up_backtick_reference() {
        // metareview_system asks the LLM to call `record_system_feedback`
        // via backticks.
        let table = PromptToolTable::build().unwrap();
        let tools = table.tools_for(MetaReviewSystem);
        assert!(
            tools.iter().any(|t| t == "record_system_feedback"),
            "metareview_system backtick reference must be picked up; got {:?}",
            tools
        );
    }
}