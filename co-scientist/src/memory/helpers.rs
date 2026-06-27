//! Pure helpers: tokenizer, stemmer, idempotency key, citation / token formatters.
//!
//! No database, no async, no `Memory` handle. Anything that depends on the DB
//! lives in the table-specific submodules. This file's items are testable
//! directly with `cargo test --lib`.

use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::types::Context;

/// Cheap token approximation. 4 chars/token is a standard rule of
/// thumb; we round up so over-estimates err on the side of
/// "prompt is bigger than I think" which is the safer direction.
pub fn approx_tokens(s: &str) -> usize {
    if s.is_empty() {
        return 0;
    }
    (s.len() + 3) / 4
}

/// Render a citation marker. Use this in the LLM's response to
/// reference a specific semantic or behavior memory by id; the
/// reader can resolve it via `get_observation`.
pub fn cite(observation_id: i64) -> String {
    format!("[ref:{}]", observation_id)
}

/// Generate a new run id. Convenience wrapper.
pub fn new_run_id() -> String {
    Uuid::new_v4().to_string()
}

/// Compute a 16-byte hex idempotency key from any number of string parts.
/// The leading `kind` slot disambiguates keys across tables (e.g. "event"
/// vs "semantic") so the same payload in two tables does not collide.
pub fn idempotency_key(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update(b"\0");
    }
    let digest = h.finalize();
    let mut out = String::with_capacity(32);
    for b in &digest[..16] {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Tokenize text, lowercase, split on non-alphanumeric, drop
/// short stop-words, apply porter-style stemming. Used by the
/// inverted index. Stemming groups morphological variants
/// (e.g. "experiment" / "experimental" / "experimenting") into
/// a single index term, improving recall.
pub fn tokenize(text: &str) -> Vec<String> {
    const STOP: &[&str] = &[
        "the", "a", "an", "of", "to", "in", "on", "for", "and", "or", "is",
        "are", "was", "were", "be", "been", "being", "this", "that", "with",
        "as", "at", "by", "from", "it", "its",
    ];
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3)
        .map(|w| w.to_lowercase())
        .filter(|w| !STOP.contains(&w.as_str()))
        .map(|w| stem(&w))
        .collect()
}

/// Simplified porter-style stemmer. Handles the most common English
/// suffixes to group morphological variants into a single index term.
/// Conservative rules — prefers under-stemming over over-stemming.
fn stem(word: &str) -> String {
    if word.len() <= 4 {
        return word.to_string();
    }

    // Longest suffix first. Rules are ordered to avoid conflicts.
    // Each rule: (suffix, replacement, min_stem_len)
    // High min_stem prevents over-stemming of domain terms.
    let suffixes: &[(&str, &str, usize)] = &[
        // 7+ char suffixes
        ("ational", "ate", 5),
        ("tional", "tion", 5),
        ("fulness", "ful", 5),
        ("ousness", "ous", 5),
        ("iveness", "ive", 5),
        ("ization", "ize", 5),
        ("isation", "ise", 5),
        // 5-6 char suffixes
        ("ating", "ate", 5),
        ("ation", "ate", 5),
        ("alism", "al", 5),
        ("aliti", "al", 5),
        ("iviti", "ive", 5),
        ("biliti", "ble", 5),
        ("ously", "ous", 5),
        ("ently", "ent", 5),
        ("ically", "ic", 5),
        // 4 char suffixes — require longer stems
        ("ment", "", 6), // "experiment" → "experi", but "judgment" stays
        ("ness", "", 5),
        ("able", "", 5),
        ("ible", "", 5),
        ("tion", "", 6), // "optimization" → "optimize", but "mutation" stays
        ("sion", "", 6),
        ("ence", "", 5),
        ("ance", "", 5), // "resistance" → "resist"
        ("ling", "", 5),
        ("ally", "al", 5),
        ("ized", "ize", 5),
        ("ised", "ise", 5),
        ("ying", "y", 5),
        // 3 char suffixes
        ("ing", "", 5), // "learning" → "learn", "experimenting" → "experiment"
        ("ers", "", 5),
        ("ies", "y", 5), // "discoveries" → "discovery"
        ("ied", "y", 5),
        ("ous", "", 5),
        ("ive", "", 5),
        ("ful", "", 5),
        ("ity", "", 5),
        ("ent", "", 6), // "gradient" stays, "experiment" → handled by "ment" first
        ("ant", "", 5),
        ("est", "", 5),
        ("ism", "", 5),
        ("ist", "", 5),
        ("ize", "", 5),
        ("ise", "", 5),
        // 2 char suffixes — only for longer words
        ("ed", "", 6), // "affected" → "affect", but "used" stays
        ("er", "", 6),
        ("ly", "", 6),
        ("es", "", 6), // "hypotheses" → "hypothes", but "cases" stays
        ("al", "", 6), // "analytical" → "analytic", but "final" stays
    ];

    for &(suffix, replacement, min_stem) in suffixes {
        if word.ends_with(suffix) {
            let stem_len = word.len() - suffix.len();
            if stem_len >= min_stem {
                let mut result = String::with_capacity(stem_len + replacement.len());
                result.push_str(&word[..stem_len]);
                result.push_str(replacement);
                return result;
            }
        }
    }

    word.to_string()
}

/// Render the [`Context`] into the markdown-ish string the prompt sees.
///
/// Pure function over the [`Context`] value object. Lives here so it can be
/// unit-tested without a database.
pub fn render_context(ctx: &Context, max_tokens: usize, full_count: usize) -> String {
    let mut s = String::new();
    let budget = if max_tokens > 0 {
        max_tokens * 4 // chars ≈ tokens * 4
    } else {
        usize::MAX
    };

    if !ctx.semantic.is_empty() {
        s.push_str("## Relevant semantic memories\n");
        if full_count > 0 {
            s.push_str("Compact: id [scope] summary | Full: **id** [scope] summary + details\n\n");
        }
        for (i, m) in ctx.semantic.iter().enumerate() {
            let line = if i < full_count && m.details.is_some() {
                let details_str = m
                    .details
                    .as_ref()
                    .map(|v: &Value| serde_json::to_string(v).unwrap_or_default())
                    .unwrap_or_default();
                format!(
                    "- **{}** [{}] {} | {}\n",
                    m.id, m.scope, m.summary, details_str
                )
            } else {
                format!("- {} [{}] {}\n", m.id, m.scope, m.summary)
            };
            if s.len() + line.len() > budget {
                s.push_str("... (truncated)\n");
                break;
            }
            s.push_str(&line);
        }
        s.push('\n');
    }
    if !ctx.behavior.is_empty() {
        let section_start = s.len();
        s.push_str("## Behavior notes for this agent\n");
        for b in &ctx.behavior {
            let line = format!("- {}: {}\n", b.pattern, b.notes);
            if s.len() + line.len() > budget {
                s.truncate(section_start);
                break;
            }
            s.push_str(&line);
        }
        if s.len() > section_start {
            s.push('\n');
        }
    }
    if !ctx.recent_events.is_empty() {
        let section_start = s.len();
        s.push_str("## Recent events in this session\n");
        for e in ctx.recent_events.iter().rev() {
            let line = format!("- step {} {} ({})\n", e.step_index, e.r#type, e.id);
            if s.len() + line.len() > budget {
                s.truncate(section_start);
                break;
            }
            s.push_str(&line);
        }
        if s.len() > section_start {
            s.push('\n');
        }
    }
    if s.is_empty() {
        s.push_str("(no prior context)\n");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approx_tokens_handles_empty() {
        assert_eq!(approx_tokens(""), 0);
    }

    #[test]
    fn approx_tokens_rounds_up() {
        // (len + 3) / 4 — over-estimate is the safer direction.
        assert_eq!(approx_tokens("ab"), 1); // 2 chars → 1
        assert_eq!(approx_tokens("abc"), 1); // 3 chars → 1
        assert_eq!(approx_tokens("abcd"), 1); // 4 chars → 1
        assert_eq!(approx_tokens("abcde"), 2); // 5 chars → 2
        assert_eq!(approx_tokens("hello world"), 3); // 11 chars → 3
    }

    #[test]
    fn cite_formats_id_with_brackets() {
        assert_eq!(cite(42), "[ref:42]");
        assert_eq!(cite(0), "[ref:0]");
        assert_eq!(cite(999_999), "[ref:999999]");
    }

    #[test]
    fn idempotency_key_is_deterministic() {
        for parts in [
            &["event", "r1", "1", "0", "x", ""][..],
            &["semantic", "r1", "1", "experiment", "x", ""][..],
            &["task", "r1", "a", "x", "{\"k\":1}"][..],
        ] {
            assert_eq!(idempotency_key(parts), idempotency_key(parts));
        }
    }

    #[test]
    fn idempotency_key_disambiguates_payload_and_kind() {
        let a = idempotency_key(&["event", "r", "1", "0", "x", ""]);
        let b = idempotency_key(&["event", "r", "1", "0", "x", "y"]);
        let c = idempotency_key(&["semantic", "r", "1", "0", "x", ""]);
        let d = idempotency_key(&["task", "r", "1", "0", "x", ""]);
        assert_ne!(a, b, "different payload -> different key");
        assert_ne!(a, c, "different kind -> different key");
        assert_ne!(a, d);
        assert_ne!(c, d);
        assert_eq!(a.len(), 32, "16-byte hex = 32 chars");
    }

    #[test]
    fn idempotency_key_no_collisions_in_1000_random_draws() {
        use std::collections::HashSet;
        let mut seen: HashSet<String> = HashSet::new();
        for i in 0..1000 {
            let key = idempotency_key(&[
                "semantic",
                &format!("r{i}"),
                "1",
                "experiment",
                &format!("summary {i}"),
                &format!("{{\"k\":{i}}}"),
            ]);
            assert!(seen.insert(key.clone()), "collision at {i}: {key}");
        }
    }

    #[test]
    fn tokenize_lowercases_and_drops_stop_words() {
        let toks = tokenize("The KRAS-G12C binds sotorasib at CYS12");
        assert!(!toks.contains(&"the".to_string()));
        assert!(!toks.contains(&"at".to_string()));
        assert!(toks.contains(&"kras".to_string()));
        assert!(toks.contains(&"g12c".to_string()));
        assert!(toks.contains(&"binds".to_string()));
        assert!(toks.contains(&"sotorasib".to_string()));
        assert!(toks.contains(&"cys12".to_string()));
        for t in &toks {
            assert_eq!(*t, t.to_lowercase());
            assert!(t.len() >= 3);
        }
    }

    #[test]
    fn render_context_empty_returns_no_prior() {
        let ctx = Context::default();
        assert_eq!(render_context(&ctx, 0, 0), "(no prior context)\n");
    }

    #[test]
    fn render_context_respects_token_budget() {
        // 1 token = 4 chars budget; with a tiny budget the function
        // should mark truncation and stop adding semantic lines.
        let mut ctx = Context::default();
        for i in 0..5 {
            ctx.semantic.push(super::super::types::SemanticMemory {
                id: i,
                run_id: "r".into(),
                agent_id: None,
                scope: "s".into(),
                summary: format!("summary {i}"),
                details: None,
                importance: 1.0,
                archived: false,
                created_at: "now".into(),
            });
        }
        let out = render_context(&ctx, 5, 0); // ~20 chars
        assert!(out.contains("... (truncated)"));
    }

    // ---- A: idempotency_key edge cases ----

    /// An empty parts slice hashes the empty string. The result is still
    /// a valid 32-hex key — pinning this prevents a future "short-circuit
    /// on empty" change from breaking callers.
    #[test]
    fn idempotency_key_empty_parts_is_well_formed() {
        let k = idempotency_key(&[]);
        assert_eq!(k.len(), 32);
        assert!(k.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// A NUL byte inside a part shifts the key because parts are joined
    /// with `b"\0"` separators. So `"a\0b"` ≠ `"a"` + `"b"` — they produce
    /// different keys. This is the property we rely on for disambiguation.
    #[test]
    fn idempotency_key_nul_in_part_changes_key() {
        let plain = idempotency_key(&["ab"]);
        let nul_separated = idempotency_key(&["a\0b"]);
        assert_ne!(plain, nul_separated);
    }

    // ---- B: adversarial Unicode inputs ----

    /// CJK and emoji inputs should produce stable, non-colliding keys.
    /// No assertion on the exact value — just on (a) stability across
    /// calls and (b) no collision in a small adversarial batch.
    #[test]
    fn idempotency_key_handles_unicode() {
        use std::collections::HashSet;
        let inputs: Vec<Vec<&str>> = vec![
            vec!["语义", "实验"],
            vec!["🧪", "sotorasib"],
            vec!["café", "naïve"],
            vec!["🦀", "rust"],
        ];
        let mut seen: HashSet<String> = HashSet::new();
        for parts in &inputs {
            let k = idempotency_key(parts);
            assert_eq!(k.len(), 32);
            assert_eq!(k, idempotency_key(parts), "non-deterministic on {parts:?}");
            assert!(seen.insert(k.clone()), "collision in unicode batch: {k}");
        }
    }

    // ---- C: tokenize edge cases ----

    /// Empty string yields no tokens. Pure data point, but it pins the
    /// iterator-chain's behavior at the boundary.
    #[test]
    fn tokenize_empty_yields_no_tokens() {
        assert!(tokenize("").is_empty());
    }

    /// All-stop-words input yields no tokens (every term is filtered out
    /// before stemming). Common LLM preamble case.
    #[test]
    fn tokenize_only_stop_words_yields_empty() {
        assert!(tokenize("the a an of to in on for and or").is_empty());
    }

    /// Non-ASCII characters are kept together as one token by the
    /// `!is_alphanumeric()` split. CJK text is all alphanumeric so it
    /// stays as a single block.
    #[test]
    fn tokenize_keeps_cjk_as_single_tokens() {
        let toks = tokenize("KRAS抑制剂筛选");
        assert_eq!(toks.len(), 1);
        assert_eq!(toks[0], "kras抑制剂筛选");
    }

    // ---- D: stemmer conservativeness ----

    /// The stemmer is deliberately conservative. Domain words below the
    /// `min_stem` length stay whole. Pin these so a future "more
    /// aggressive" change is intentional.
    #[test]
    fn stemmer_does_not_overstem_short_words() {
        // "used" → stemmer should leave it alone (suffix "ed", min_stem 6,
        // but "used" stem_len is 2 < 6, so no rule fires).
        assert!(tokenize("used").contains(&"used".to_string()));
        // "final" → suffix "al" with min_stem 6; stem_len 3 < 6, no rule.
        assert!(tokenize("final").contains(&"final".to_string()));
        // "judgment" → suffix "ment" with min_stem 6; stem_len 4 < 6, no rule.
        assert!(tokenize("judgment").contains(&"judgment".to_string()));
    }

    // ---- E: render_context edge cases ----

    /// The render order is semantic → behavior → events. Section
    /// ordering is the contract with the LLM.
    #[test]
    fn render_context_section_ordering() {
        let mut ctx = Context::default();
        ctx.semantic.push(semantic("kras", 1));
        ctx.behavior.push(behavior("n=10", 2));
        ctx.recent_events.push(event(0, "turn_started", 3));
        let out = render_context(&ctx, 0, 0);
        let sem_pos = out.find("## Relevant semantic memories").unwrap();
        let beh_pos = out.find("## Behavior notes").unwrap();
        let evt_pos = out.find("## Recent events").unwrap();
        assert!(sem_pos < beh_pos, "semantic must come before behavior");
        assert!(beh_pos < evt_pos, "behavior must come before events");
    }

    /// `full_count = 0` means *no* semantic entry gets the full detail
    /// treatment, even when details are present.
    #[test]
    fn render_context_full_count_zero_compacts_all_semantic() {
        let mut ctx = Context::default();
        ctx.semantic.push(semantic_with_details("kras binds sotorasib", 1, r#"{"k":1}"#));
        let out = render_context(&ctx, 0, 0);
        // Compact form: no bold **id**, no JSON-pipe detail.
        assert!(!out.contains("**1**"), "no entry should be bolded");
        assert!(!out.contains(r#""k":1""#), "details must not appear");
        assert!(out.contains("- 1 [scope] kras binds sotorasib"));
    }

    /// Events section, on budget overflow, *truncates the whole
    /// section* back to `section_start` (unlike semantic which marks
    /// truncation). Pin this asymmetry.
    #[test]
    fn render_context_events_overflow_truncates_section() {
        let mut ctx = Context::default();
        // Many events to blow the budget.
        for i in 0..20 {
            ctx.recent_events.push(event(i, "turn_started_long_label", 100 + i));
        }
        let out = render_context(&ctx, 5, 0); // ~20 chars total
        // The events header should NOT appear because the section was
        // truncated away.
        assert!(!out.contains("## Recent events"), "events section was truncated");
    }

    // ---- helpers for the tests above ----

    fn semantic(summary: &str, id: i64) -> super::super::types::SemanticMemory {
        super::super::types::SemanticMemory {
            id,
            run_id: "r".into(),
            agent_id: None,
            scope: "scope".into(),
            summary: summary.into(),
            details: None,
            importance: 1.0,
            archived: false,
            created_at: "now".into(),
        }
    }

    fn semantic_with_details(
        summary: &str,
        id: i64,
        details_json: &str,
    ) -> super::super::types::SemanticMemory {
        let details = serde_json::from_str(details_json).ok();
        super::super::types::SemanticMemory {
            id,
            run_id: "r".into(),
            agent_id: None,
            scope: "scope".into(),
            summary: summary.into(),
            details,
            importance: 1.0,
            archived: false,
            created_at: "now".into(),
        }
    }

    fn behavior(notes: &str, id: i64) -> super::super::types::BehaviorMemory {
        super::super::types::BehaviorMemory {
            id,
            agent_id: 1,
            pattern: "pattern".into(),
            notes: notes.into(),
            evidence: None,
            created_at: "now".into(),
        }
    }

    fn event(step: i64, type_: &str, id: i64) -> super::super::types::Event {
        super::super::types::Event {
            id,
            run_id: "r".into(),
            agent_id: 1,
            step_index: step,
            r#type: type_.into(),
            payload: None,
            created_at: "now".into(),
        }
    }
}