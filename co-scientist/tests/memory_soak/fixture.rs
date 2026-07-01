//! Realistic research observation fixtures.
//!
//! Designed to *resemble real-world tasks*: the actual 7-agent pipeline
//! (supervisor → generation → reflection → ranking → evolution →
//! metareview → experiment) over a research goal. Each fixture is a
//! tuple of `(scope, agent, summary, details_json)` shaped like what
//! those agents would actually produce.
//!
//! Why static fixtures instead of faker-style random strings?
//! The whole point of this benchmark is to be reproducible — if the
//! retrieval regresses, I want to see the *same* queries fail on the
//! *same* fixtures, not random flakes. So: deterministic data, with a
//! small RNG layer that picks which subset to use per workload.

use serde_json::json;

/// The five scopes the memory layer accepts (see `save_semantic`).
/// These mirror real research workflow stages.
pub const SCOPES: &[&str] = &["experiment", "insight", "result", "question", "plan"];

/// The seven agents in the research pipeline. Matches the topology
/// recorded in the harness readiness report.
pub const AGENTS: &[&str] = &[
    "supervisor",
    "generation",
    "reflection",
    "ranking",
    "evolution",
    "metareview",
    "experiment",
];

/// Domain tags we mix across fixtures. Used to drive the
/// recall-precision probes — a query like "KRAS" should surface
/// KRAS-tagged memories, not BRCA-tagged ones.
pub const TOPICS: &[&str] = &[
    "KRAS-G12C",
    "BRCA1",
    "mTOR",
    "EGFR",
    "PD-L1",
    "transformer",
    "RLHF",
    "MoE",
    "KV-cache",
    "speculative-decoding",
    "graph-neural-net",
    "diffusion",
    "catalysis",
    "organometallic",
    "CRISPR-Cas9",
    "alpha-fold",
    "protein-folding",
    "quantum-error-correction",
    "topological",
    "Fermi-liquid",
];

#[derive(Clone, Debug)]
pub struct Observation {
    pub scope: &'static str,
    pub agent: &'static str,
    pub summary: String,
    pub details: serde_json::Value,
    pub topic: &'static str,
}

/// One realistic research observation, deterministic index.
pub fn observation(idx: usize) -> Observation {
    let scope = SCOPES[idx % SCOPES.len()];
    let agent = AGENTS[idx % AGENTS.len()];
    let topic = TOPICS[(idx / 3) % TOPICS.len()];
    let summary = match (scope, topic) {
        ("experiment", t) => format!("Ran protocol EP-{idx:04} on {t}: observed dose-response curve with EC50 in expected range"),
        ("insight", t) => format!("{t} shows non-obvious cross-talk with adjacent pathway; worth a follow-up probe"),
        ("result", t) => format!("Result R-{idx:04}: {t} effect size d=0.{:02} (95% CI excludes null)", (idx % 90) + 10),
        ("question", t) => format!("Is the {t} effect reproducible across cell lines, or batch-specific?"),
        ("plan", t) => format!("Plan P-{idx:04}: replicate {t} finding with n=30, pre-registered analysis"),
        _ => format!("Generic note {idx}"),
    };
    let details = json!({
        "topic": topic,
        "iteration": idx,
        "metric": {
            "effect_size": 0.1 + (idx % 100) as f64 / 1000.0,
            "p_value": (idx % 99) as f64 / 1e6,
            "n": 10 + (idx % 90),
        },
        "tags": [topic, scope, agent],
    });
    Observation { scope, agent, summary, details, topic }
}

/// A multi-turn research session: supervisor creates plan → 3 hypotheses
/// ranked → top hypothesis gets an experiment → result → reflection.
pub fn research_session(idx: usize) -> Vec<Observation> {
    let plan = observation(idx * 7);
    let h1 = observation(idx * 7 + 1);
    let h2 = observation(idx * 7 + 2);
    let h3 = observation(idx * 7 + 3);
    let exp = observation(idx * 7 + 4);
    let res = observation(idx * 7 + 5);
    let refl = observation(idx * 7 + 6);
    vec![plan, h1, h2, h3, exp, res, refl]
}

/// A query that should match a specific topic's observations.
/// Used by the recall-precision probe. The query string contains
/// the topic token plus 1–2 contextual modifiers drawn from a
/// fixed pool so retrieval has to do real lexical work, not exact match.
pub fn query_for_topic(topic: &str) -> String {
    let modifiers = ["resistance", "binding affinity", "dose response", "reproducibility", "mechanism", "off-target"];
    let m1 = modifiers[topic.len() % modifiers.len()];
    let m2 = modifiers[(topic.len() / 2 + 1) % modifiers.len()];
    format!("{topic} {m1} {m2}")
}

/// Generate a near-duplicate paraphrase of an existing summary.
/// Used to exercise the dedup-threshold probe at the boundary.
pub fn paraphrase(s: &str) -> String {
    // Simple deterministic substitution: swap "observed" → "found",
    // "shows" → "demonstrates", etc. If none match, append a synonym.
    let mut out = s.to_string();
    for (a, b) in [
        ("observed", "found"),
        ("shows", "demonstrates"),
        ("non-obvious", "unexpected"),
        ("dose-response", "concentration-dependent"),
        ("EC50", "half-maximal effective concentration"),
    ] {
        if out.contains(a) {
            return out.replacen(a, b, 1);
        }
    }
    out.push_str(" (paraphrased variant)");
    out
}

/// Edge-case strings: empty, very long, unicode, control bytes, etc.
/// Used by the edge-input probe.
pub fn edge_summaries() -> Vec<(&'static str, &'static str)> {
    vec![
        ("empty", ""),
        ("single_char", "x"),
        ("only_stopwords", "the a an of and or but"),
        ("unicode_emoji", "🧬 KRAS-G12C binds sotorasib 🔬 — observed ✅"),
        ("unicode_cjk", "研究显示 KRAS-G12C 与 sotorasib 结合"),
        ("unicode_rtl", "البروتين KRAS-G12C يرتبط بـ sotorasib"),
        ("control_bytes", "before\x00after\x01\x02ctrl"),
        // Leak the 10 KB string so it satisfies `&'static str`. This
        // runs once at startup; the leak is bounded (~10 KB).
        ("very_long", Box::leak("x".repeat(10_000).into_boxed_str())),
        ("only_punct", "...?!;:"),
        ("newlines", "line1\nline2\nline3"),
        ("sql_like_injection", "x'; DROP TABLE semantic_memories; --"),
        ("path_traversal", "../../../etc/passwd"),
        ("json_in_summary", "{\"scope\":\"hack\"}"),
    ]
}

/// Sanity check on a query–observation pair: does this observation
/// match this query lexically? Used as the ground truth for
/// recall-precision probes. Conservative — only counts tokens that
/// appear in both.
pub fn lexical_match(query: &str, summary: &str) -> bool {
    let q_tokens: std::collections::HashSet<String> = query
        .split_whitespace()
        .map(|w| w.to_lowercase())
        .filter(|w| w.len() > 2)
        .collect();
    let s_tokens: std::collections::HashSet<String> = summary
        .split_whitespace()
        .map(|w| w.to_lowercase())
        .filter(|w| w.len() > 2)
        .collect();
    if q_tokens.is_empty() {
        return false;
    }
    let shared = q_tokens.intersection(&s_tokens).count();
    // Match if at least the topic token is present.
    shared >= 1
}