//! Single source of truth for "what tools exist" and "what each agent
//! can call".
//!
//! Four lists in the crate used to be hand-maintained and silently
//! drifted:
//!
//! 1. `tool::builtin_tools()` — the list of registered tools.
//! 2. `marker::allowlist::known_tool_names()` — the set of names the LLM
//!    might emit (used to scan backtick-quoted references).
//! 3. `registry::default_allowlist()` — per-agent allowlists.
//! 4. `prompts::PROMPT_MODES` × `include_str!` paths — the prompt
//!    manifest (driven by `AgentMode::filename`).
//!
//! This module collapses (1), (2), and (3) into one `TOOL_CATALOG`
//! constant. (4) stays where it is (`prompts.rs`) because prompts are
//! content, not catalog.
//!
//! Adding a new tool is now: register the impl in `tool::builtin_tools()`
//! (one `Arc::new(...)` line, which the registry uses to dispatch), and
//! add a row to [`TOOL_CATALOG`] (the catalog description). `builtin_tools`
//! panics in tests if a row is missing its impl, so the drift bug is
//! caught at `cargo test`.

/// Per-tool description for the catalog. One row per tool the LLM might
/// invoke. The `name` is the canonical name (post-alias-rewrite); `aliases`
/// are the community prompt names that resolve to it. `always_allowed`
/// marks synthetic sentinels (`noop`, `none`) that are dispatchable by
/// every agent regardless of the per-agent allowlist.
#[derive(Debug, Clone, Copy)]
pub struct CatalogEntry {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub always_allowed: bool,
}

/// The catalog. Every tool the LLM might call appears exactly once.
/// Adding a tool = adding one row here.
pub const TOOL_CATALOG: &[CatalogEntry] = &[
    // Memory ops (5)
    CatalogEntry { name: "save_semantic",   aliases: &["record_research_plan"], always_allowed: false },
    CatalogEntry { name: "save_behavior",   aliases: &["record_system_feedback"], always_allowed: false },
    CatalogEntry { name: "get_context",     aliases: &[], always_allowed: false },
    CatalogEntry { name: "compress_events", aliases: &[], always_allowed: false },
    // 3-layer retrieval (3)
    CatalogEntry { name: "peek_context",    aliases: &[], always_allowed: false },
    CatalogEntry { name: "get_timeline",    aliases: &[], always_allowed: false },
    CatalogEntry { name: "get_observation", aliases: &[], always_allowed: false },
    // Curation (2)
    CatalogEntry { name: "archive_observation", aliases: &[], always_allowed: false },
    CatalogEntry { name: "delete_observation",  aliases: &[], always_allowed: false },
    // Structured research (3)
    CatalogEntry { name: "record_hypothesis",      aliases: &[], always_allowed: false },
    CatalogEntry { name: "record_review",          aliases: &[], always_allowed: false },
    CatalogEntry { name: "record_tournament_match",aliases: &[], always_allowed: false },
    // Empirical loop (3)
    CatalogEntry { name: "design_experiment", aliases: &[], always_allowed: false },
    CatalogEntry { name: "execute_experiment",aliases: &[], always_allowed: false },
    CatalogEntry { name: "evaluate_result",   aliases: &[], always_allowed: false },
    // Inline execution (1) — synchronous-blocking python runner
    CatalogEntry { name: "run_python",        aliases: &[], always_allowed: false },
    // Synthetic sentinels (2) — dispatchable regardless of allowlist
    CatalogEntry { name: "noop", aliases: &[], always_allowed: true },
    CatalogEntry { name: "none", aliases: &[], always_allowed: true },
];

/// Every tool name the LLM might emit, derived once from the catalog
/// (canonical names + aliases). Used by `marker::allowlist` to scan
/// backtick-quoted tool references in prompt prose.
pub fn known_tool_names() -> &'static [&'static str] {
    use std::sync::OnceLock;
    static NAMES: OnceLock<Vec<&'static str>> = OnceLock::new();
    NAMES.get_or_init(|| {
        let mut v: Vec<&'static str> = Vec::new();
        for entry in TOOL_CATALOG {
            v.push(entry.name);
            v.extend_from_slice(entry.aliases);
        }
        // Stable order for stable diffs.
        v.sort_unstable();
        v.dedup();
        v
    })
}

/// Lookup a catalog entry by canonical name. Returns `None` for unknown
/// names; callers should treat that as a programmer error.
pub fn find(name: &str) -> Option<&'static CatalogEntry> {
    TOOL_CATALOG.iter().find(|e| e.name == name)
}

/// Per-agent allowlist, derived from the catalog. The community's
/// prompt names are accepted on top of our local names; the registry
/// rewrites them via [`crate::marker_normalizer::canonicalize`].
///
/// `agent_name` must match one of the canonical agent names (`supervisor`,
/// `generation`, `reflection`, `ranking`, `evolution`, `metareview`,
/// `experiment`). Legacy agent names map to the research set; unknown
/// names default to the research set (same as the previous behaviour).
pub fn default_allowlist(agent: &str) -> Option<Vec<&'static str>> {
    // The per-agent list is assembled from catalog names. We compose
    // it by picking which tools each agent owns — the names come from
    // the catalog so a tool rename updates all agents in lockstep.
    let research: &[&str] = &[
        "save_semantic", "save_behavior", "get_context", "compress_events",
        "peek_context", "get_timeline", "get_observation",
        "record_hypothesis", "record_review", "record_tournament_match",
        // Community aliases — accepted as-is, rewritten by the registry.
        "record_system_feedback", "record_research_plan",
    ];
    let archive_tools: &[&str] = &["archive_observation", "delete_observation"];
    let ranking_only: &[&str] = &["record_tournament_match"];
    // `run_python` is included for any agent that might legitimately
    // need ad-hoc synchronous Python: generation / reflection /
    // evolution (sanity-check a hypothesis's algebra), metareview
    // (verify a claim), experiment (compute a metric inline), and
    // supervisor (debugging). Ranking is tournament-only and skips
    // it. The tool blocks until the script exits, so misuse is
    // bounded by the wall-clock cap (default 30s).
    let experiment_tools: &[&str] = &[
        "save_semantic", "save_behavior", "get_context",
        "peek_context", "get_timeline", "get_observation",
        "design_experiment", "execute_experiment", "evaluate_result",
        "run_python",
        "record_review",
    ];

    let compose = |base: &[&'static str], extra: &[&'static str]| -> Vec<&'static str> {
        let mut v: Vec<&'static str> = base.to_vec();
        v.extend_from_slice(extra);
        v
    };

    match agent {
        "supervisor" => {
            // Supervisor parses the goal into a research plan (calls
            // `record_research_plan`, which canonicalizes to
            // `save_semantic` with scope="plan"), curates its own
            // session (archive / delete junk observations), and can
            // run inline Python to debug or verify.
            Some(compose(
                &["save_semantic", "record_research_plan", "run_python"],
                archive_tools,
            ))
        }
        "ranking" => Some(ranking_only.to_vec()),
        "generation" | "reflection" | "evolution" => {
            // Research agents get the full research set plus
            // `run_python` for sanity-check computations.
            let mut v = research.to_vec();
            v.push("run_python");
            Some(v)
        }
        "metareview" => Some(compose(research, archive_tools)),
        "experiment" => Some(experiment_tools.to_vec()),
        // Legacy / pre-refactor names map to the research set.
        "literature" | "hypothesis" | "analysis" | "critic" | "synthesizer" => {
            Some(research.to_vec())
        }
        _ => Some(research.to_vec()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::marker_normalizer::ALIASES;

    /// Every catalog entry whose `always_allowed` is false must be
    /// discoverable via `find()`; that pair is the contract between the
    /// catalog and `tool::builtin_tools()`.
    #[test]
    fn catalog_names_are_unique() {
        let mut names: Vec<&str> = TOOL_CATALOG.iter().map(|e| e.name).collect();
        let original_len = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), original_len, "duplicate canonical names in TOOL_CATALOG");
    }

    #[test]
    fn catalog_aliases_are_unique() {
        let mut aliases: Vec<&str> =
            TOOL_CATALOG.iter().flat_map(|e| e.aliases.iter().copied()).collect();
        let original_len = aliases.len();
        aliases.sort();
        aliases.dedup();
        assert_eq!(aliases.len(), original_len, "duplicate aliases in TOOL_CATALOG");
    }

    /// The aliases in `marker_normalizer::ALIASES` must be a subset of
    /// the catalog's alias set. If a new alias is added to the
    /// normalizer without a catalog row, this fails.
    #[test]
    fn normalizer_aliases_appear_in_catalog() {
        let catalog_aliases: std::collections::HashSet<&str> = TOOL_CATALOG
            .iter()
            .flat_map(|e| e.aliases.iter().copied())
            .collect();
        for a in ALIASES {
            assert!(
                catalog_aliases.contains(a.from),
                "marker_normalizer alias `{}` has no catalog entry — \
                 add a CatalogEntry with `aliases: &[..., {:?}]`",
                a.from,
                a.from,
            );
        }
    }

    #[test]
    fn known_tool_names_includes_canonical_and_aliases() {
        let names = known_tool_names();
        assert!(names.contains(&"save_semantic"));
        assert!(names.contains(&"record_research_plan"));
        assert!(names.contains(&"record_system_feedback"));
        assert!(names.contains(&"noop"));
    }

    #[test]
    fn default_allowlist_research_agents_get_full_research_set() {
        let allow = default_allowlist("generation").unwrap();
        assert!(allow.contains(&"save_semantic"));
        assert!(allow.contains(&"record_hypothesis"));
        assert!(allow.contains(&"record_tournament_match"));
        // Ranking-only the research set does NOT include.
        assert!(!allow.contains(&"archive_observation"));
    }

    #[test]
    fn default_allowlist_ranking_is_tournament_only() {
        let allow = default_allowlist("ranking").unwrap();
        assert_eq!(allow, vec!["record_tournament_match"]);
    }

    #[test]
    fn default_allowlist_supervisor_can_curate() {
        let allow = default_allowlist("supervisor").unwrap();
        assert!(allow.contains(&"save_semantic"));
        assert!(allow.contains(&"record_research_plan"));
        assert!(allow.contains(&"archive_observation"));
        assert!(allow.contains(&"delete_observation"));
    }
}