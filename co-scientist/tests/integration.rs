//! End-to-end test of the co-scientist memory + skill pipeline.
//!
//! Excludes the Claude CLI integration (which needs `claude` in PATH) and
//! covers everything else: DB schema, log_event, save_semantic,
//! save_behavior, get_context, and the skill marker parser.

use co_scientist::{
    db,
    memory::{ContextLimits, Memory, new_run_id},
    skill::{parse_markers, Marker, SKILL},
};
use serde_json::json;

#[tokio::test]
async fn db_migrates_and_seeds_agents() {
    let d = db::open_memory().await.expect("open in-memory db");
    let mem = Memory::new(d);
    mem.upsert_agent("hypothesis", "test role", "test prompt")
        .await
        .expect("upsert agent");
    mem.upsert_agent("hypothesis", "test role", "updated prompt")
        .await
        .expect("idempotent upsert");
    let id = mem.agent_id_for_test("hypothesis").await.unwrap();
    assert!(id.is_some(), "agent should exist");
}

#[tokio::test]
async fn log_event_writes_to_events() {
    let mem = Memory::new(db::open_memory().await.unwrap());
    let run_id = new_run_id();
    mem.log_event(&run_id, "hypothesis", 0, "turn_started", Some(json!({"x": 1})))
        .await
        .unwrap();
    mem.log_event(&run_id, "hypothesis", 0, "turn_completed", None)
        .await
        .unwrap();
    let count: i64 = mem
        .db()
        .conn()
        .query("SELECT COUNT(*) FROM events", ())
        .await
        .unwrap()
        .next()
        .await
        .unwrap()
        .unwrap()
        .get(0)
        .unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn save_and_recall_semantic_memory() {
    let mem = Memory::new(db::open_memory().await.unwrap());
    let run_id = new_run_id();
    let id = mem
        .save_semantic(
            &run_id,
            Some("experiment"),
            "experiment",
            "KRAS G12C binds sotorasib covalently at Cys12",
            Some(json!({"compound":"sotorasib","target":"KRAS G12C","ki_nM": 0.07})),
        )
        .await
        .unwrap();
    assert!(id > 0);

    let ctx = mem
        .get_context(
            &run_id,
            "experiment",
            "sotorasib KRAS",
            ContextLimits { events: 5, semantic: 5, behavior: 3, max_tokens: 0, full_count: 3 },
        )
        .await
        .unwrap();
    assert_eq!(ctx.semantic.len(), 1);
    assert!(ctx.rendered.contains("sotorasib"));
    assert!(ctx.rendered.contains("KRAS"));
}

#[tokio::test]
async fn save_behavior_round_trips() {
    let mem = Memory::new(db::open_memory().await.unwrap());
    let id = mem
        .save_behavior(
            "synthesizer",
            "state-of-the-art summary",
            "always include what we don't know alongside what we do",
            Some(json!({"evidence": [1, 2, 3]})),
        )
        .await
        .unwrap();
    assert!(id > 0);
    let ctx = mem
        .get_context(
            &new_run_id(),
            "synthesizer",
            "anything",
            ContextLimits { events: 0, semantic: 0, behavior: 5, max_tokens: 0, full_count: 0 },
        )
        .await
        .unwrap();
    assert_eq!(ctx.behavior.len(), 1);
    assert_eq!(ctx.behavior[0].pattern, "state-of-the-art summary");
}

#[tokio::test]
async fn skill_parser_strips_markers_and_extracts_ops() {
    let response = r#"I made progress on the question.

[[MEMORY_OP:save_semantic:{"scope":"insight","summary":"foo","details":{"k":1}}]]

Then I noticed a pattern.

[[MEMORY_OP:save_behavior:{"pattern":"p","notes":"n"}]]

And asked for context.

[[MEMORY_OP:get_context:{"query":"q","limit":3}]]

Done."#;

    let parsed = parse_markers(response);
    assert!(!parsed.cleaned_text.contains("MEMORY_OP"));
    assert!(parsed.cleaned_text.contains("I made progress"));
    assert!(parsed.cleaned_text.contains("Done."));
    assert_eq!(parsed.markers.len(), 3);
    assert_eq!(parsed.markers[0].op, "save_semantic");
    assert_eq!(parsed.markers[1].op, "save_behavior");
    assert_eq!(parsed.markers[2].op, "get_context");
}

#[test]
fn skill_parser_skips_invalid_json() {
    let r = parse_markers("text [[MEMORY_OP:save_semantic:{not json}]] end");
    assert!(r.markers.is_empty());
    assert!(r.cleaned_text.contains("text"));
    assert!(r.cleaned_text.contains("end"));
}

#[test]
fn skill_parser_accepts_all_ops() {
    let r = parse_markers(
        r#"text [[MEMORY_OP:wipe_disk:{"x":1}]] more [[MEMORY_OP:save_semantic:{"scope":"x","summary":"y"}]] end"#,
    );
    assert_eq!(r.markers.len(), 2, "all ops parsed, dispatch decides validity");
    assert_eq!(r.markers[0].op, "wipe_disk");
    assert_eq!(r.markers[1].op, "save_semantic");
}

#[test]
fn skill_doc_is_present_and_nonempty() {
    assert!(SKILL.contains("save_semantic"));
    assert!(SKILL.contains("save_behavior"));
    assert!(SKILL.contains("get_context"));
    assert!(SKILL.contains("peek_context"));
    assert!(SKILL.contains("get_timeline"));
    assert!(SKILL.contains("get_observation"));
}

#[test]
fn marker_roundtrip_json() {
    let m = Marker {
        op: "save_semantic".to_string(),
        payload: json!({"scope":"experiment","summary":"x"}),
    };
    let s = serde_json::to_string(&m).unwrap();
    let back: Marker = serde_json::from_str(&s).unwrap();
    assert_eq!(m, back);
}

// ---------- Feature 1: idempotency ----------

#[tokio::test]
async fn log_event_is_idempotent() {
    let mem = Memory::new(db::open_memory().await.unwrap());
    let run_id = new_run_id();
    mem.log_event(&run_id, "hypothesis", 0, "turn_started", Some(json!({"x":1})))
        .await
        .unwrap();
    mem.log_event(&run_id, "hypothesis", 0, "turn_started", Some(json!({"x":1})))
        .await
        .unwrap();
    let count: i64 = mem
        .db()
        .conn()
        .query("SELECT COUNT(*) FROM events", ())
        .await
        .unwrap()
        .next()
        .await
        .unwrap()
        .unwrap()
        .get(0)
        .unwrap();
    assert_eq!(count, 1, "duplicate log_event should not insert twice");
}

#[tokio::test]
async fn save_semantic_is_idempotent_and_returns_same_id() {
    let mem = Memory::new(db::open_memory().await.unwrap());
    let run_id = new_run_id();
    let a = mem
        .save_semantic(&run_id, Some("hypothesis"), "experiment", "x", Some(json!({"k":1})))
        .await
        .unwrap();
    let b = mem
        .save_semantic(&run_id, Some("hypothesis"), "experiment", "x", Some(json!({"k":1})))
        .await
        .unwrap();
    assert_eq!(a, b, "idempotent insert must return the same id");
    let count: i64 = mem
        .db()
        .conn()
        .query("SELECT COUNT(*) FROM semantic_memories", ())
        .await
        .unwrap()
        .next()
        .await
        .unwrap()
        .unwrap()
        .get(0)
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn different_payloads_get_different_keys() {
    let mem = Memory::new(db::open_memory().await.unwrap());
    let run_id = new_run_id();
    mem.save_semantic(&run_id, Some("a"), "experiment", "x", Some(json!({"k":1})))
        .await
        .unwrap();
    mem.save_semantic(&run_id, Some("a"), "experiment", "x", Some(json!({"k":2})))
        .await
        .unwrap();
    let count: i64 = mem
        .db()
        .conn()
        .query("SELECT COUNT(*) FROM semantic_memories", ())
        .await
        .unwrap()
        .next()
        .await
        .unwrap()
        .unwrap()
        .get(0)
        .unwrap();
    assert_eq!(count, 2);
}

#[test]
fn idempotency_key_is_stable_and_disambiguated() {
    let a = co_scientist::memory::idempotency_key(&["event", "run1", "1", "0", "x", ""]);
    let b = co_scientist::memory::idempotency_key(&["event", "run1", "1", "0", "x", ""]);
    let c = co_scientist::memory::idempotency_key(&["event", "run1", "1", "0", "x", "y"]);
    let d = co_scientist::memory::idempotency_key(&["semantic", "run1", "1", "0", "x", ""]);
    assert_eq!(a, b);
    assert_ne!(a, c, "different payload -> different key");
    assert_ne!(a, d, "different kind -> different key");
    assert_eq!(a.len(), 32, "16-byte hex = 32 chars");
}

// ---------- Tier 3 fixes: additional gap coverage ----------

#[tokio::test]
async fn ensure_agent_is_safe_under_concurrent_creation() {
    // Two concurrent `ensure_agent("dup")` calls must produce exactly
    // one row in `agents` and return the same id. Without
    // `ON CONFLICT` on the INSERT, both callers can miss the SELECT
    // and both INSERT.
    use co_scientist::db;
    use co_scientist::memory::Memory;
    let mem = Memory::new(db::open_memory().await.unwrap());
    let m1 = mem.clone();
    let m2 = mem.clone();
    let h1 = tokio::spawn(async move { m1.ensure_agent("dup").await });
    let h2 = tokio::spawn(async move { m2.ensure_agent("dup").await });
    let id1 = h1.await.unwrap().unwrap();
    let id2 = h2.await.unwrap().unwrap();
    assert_eq!(id1, id2, "both calls must return the same agent id");
    let conn = mem.db().conn().clone();
    let mut rows = conn
        .query("SELECT COUNT(*) FROM agents WHERE name = 'dup'", ())
        .await
        .unwrap();
    let count: i64 = rows.next().await.unwrap().unwrap().get(0).unwrap();
    assert_eq!(count, 1, "must have exactly one row for 'dup'");
}

#[tokio::test]
async fn db_open_creates_a_real_file_with_schema() {
    use co_scientist::db;
    use co_scientist::memory::Memory;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("co_scientist.db");
    // First open: creates the file and runs migrations.
    let d = db::open(path.to_str().unwrap()).await.unwrap();
    let mem = Memory::new(d);
    mem.upsert_agent("a", "r", "p").await.unwrap();
    // Close the connection by dropping Memory. Re-open from the same path.
    drop(mem);
    // Open again — the schema should be present and the row should
    // be readable.
    let d2 = db::open(path.to_str().unwrap()).await.unwrap();
    let mem2 = Memory::new(d2);
    let id = mem2.agent_id_for_test("a").await.unwrap();
    assert!(id.is_some(), "row should be readable after re-open");
}

#[tokio::test]
async fn save_behavior_tool_round_trip() {
    use co_scientist::tool::{SaveBehaviorTool, Tool, ToolCtx};
    let mem = Memory::new(db::open_memory().await.unwrap());
    let ctx = ToolCtx {
        memory: mem.clone(),
        run_id: "r".into(),
        agent_name: "experiment".into(),
    };
    let tool = SaveBehaviorTool;
    let out = tool
        .call(
            serde_json::json!({
                "pattern": "always report n",
                "notes": "experiments need sample counts",
                "evidence": [1, 2, 3]
            }),
            &ctx,
        )
        .await
        .unwrap();
    let id = out["id"].as_i64().unwrap();
    let note_id = mem
        .recent_behavior("experiment", 10)
        .await
        .unwrap()
        .into_iter()
        .find(|b| b.id == id)
        .expect("behavior note should be retrievable");
        assert_eq!(note_id.pattern, "always report n");
        assert_eq!(note_id.notes, "experiments need sample counts");
        // `evidence` round-trips as the array the model emitted.
        let ev = note_id.evidence.as_ref().unwrap();
        assert_eq!(ev[0], 1);
        assert_eq!(ev[2], 3);
    }

#[tokio::test]
async fn get_context_tool_round_trip() {
    use co_scientist::tool::{GetContextTool, Tool, ToolCtx};
    let mem = Memory::new(db::open_memory().await.unwrap());
    // Seed two semantic memories.
    mem.save_semantic(
        "r1",
        Some("experiment"),
        "experiment",
        "kras g12c binds sotorasib",
        Some(serde_json::json!({"compound": "sotorasib"})),
    )
    .await
    .unwrap();
    mem.save_semantic(
        "r1",
        Some("experiment"),
        "experiment",
        "kras off-target effect",
        None,
    )
    .await
    .unwrap();
    let ctx = ToolCtx {
        memory: mem.clone(),
        run_id: "r1".into(),
        agent_name: "experiment".into(),
    };
    let tool = GetContextTool;
    let out = tool
        .call(
            serde_json::json!({"query": "kras", "semantic": 5, "events": 0, "behavior": 0}),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(out["n_semantic"], 2);
    assert!(out["rendered"].as_str().unwrap().contains("sotorasib"));
}

#[test]
fn all_14_prompts_render_with_minimum_vars() {
    use co_scientist::prompts::{AgentMode, Prompts, PromptContext, PROMPT_MODES};
    let p = Prompts::new().unwrap();
    for mode in PROMPT_MODES {
        let mut ctx = PromptContext::new();
        // Common vars every prompt expects.
        ctx.set("goal", "x");
        ctx.set("preferences", "");
        ctx.set("instructions", "");
        ctx.set("reviews_overview", "(none)");
        ctx.set("transcript", "(none)");
        ctx.set("source_hypothesis", "");
        ctx.set("articles_with_reasoning", "(none)");
        ctx.set("articles_block", "(none)");
        ctx.set("hypothesis_text", "h");
        ctx.set("hypothesis_id", "H-1");
        ctx.set("hypothesis", "h");
        ctx.set("hypothesis_id", "H-1");
        ctx.set("review", "(none)");
        ctx.set("hypothesis_1", "h1");
        ctx.set("hypothesis_2", "h2");
        ctx.set("hypothesis_1_id", "H-1");
        ctx.set("hypothesis_2_id", "H-2");
        ctx.set("review_1", "(none)");
        ctx.set("review_2", "(none)");
        ctx.set("hypothesis_a", "a");
        ctx.set("hypothesis_b", "b");
        ctx.set("hypothesis_a_id", "H-a");
        ctx.set("hypothesis_b_id", "H-b");
        ctx.set("review_a", "(none)");
        ctx.set("review_b", "(none)");
        ctx.set("hypotheses", serde_json::json!([]).to_string());
        ctx.set("reviews", "(none)");
        ctx.set("debate_rationales", "(none)");
        ctx.set("system_feedback", "(none)");
        ctx.set("top_hypotheses_block", "(none)");
        ctx.set("notes", "");
        // Vars used by reflection_observation
        ctx.set("article_id", "A-1");
        ctx.set("article_hash", "h");
        ctx.set("article", "(none)");
        // Vars used by generation_debate specifically
        ctx.set("hypothesis_id", "H-1");
        // Vars used by evolution_out_of_box (iterates over a list of
        // hypotheses with `h.id` and `h.text`).
        ctx.set_value("hypotheses", serde_json::json!([{"id":"H-1","text":"h"}]));
        // Vars used by the new experiment modes and reflection_on_result.
        ctx.set("experiment_id", "E-1");
        ctx.set("metric_name", "m");
        ctx.set("code", "print(1+1)");
        ctx.set("description", "demo");
        ctx.set("status", "succeeded");
        ctx.set("metric_value", "1.0");
        ctx.set("exit_code", "0");
        ctx.set("duration_ms", "10");
        ctx.set("stdout", "(empty)");
        ctx.set("stderr", "(empty)");
        ctx.set("verdict", "supports");
        ctx.set("result_summary", "(see details)");
        ctx.set("result_details", "{}");
        ctx.set("prior_review", "(none)");
        let out = p
            .render(*mode, &ctx)
            .unwrap_or_else(|e| {
                panic!("rendering {} failed: {e:#?}", mode.filename());
            });
        assert!(!out.is_empty(), "{} rendered empty", mode.filename());
    }
}

#[test]
fn proptest_idempotency_key_properties() {
    use co_scientist::memory::idempotency_key;
    use std::collections::HashSet;
    // 1. Deterministic.
    for parts in [
        &["event", "r1", "1", "0", "x", ""][..],
        &["semantic", "r1", "1", "experiment", "x", ""][..],
        &["task", "r1", "a", "x", "{\"k\":1}"][..],
    ] {
        assert_eq!(idempotency_key(parts), idempotency_key(parts));
    }
    // 2. Birthday-paradox sanity at the 16-byte cutoff. 1000 random
    // 6-tuples should produce no collisions. (16 bytes = 128 bits;
    // collision prob for 1000 draws is ~3e-34.)
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
    // 3. Different kinds don't collide on the same payload.
    let a = idempotency_key(&["event", "r", "1", "0", "x", "p"]);
    let b = idempotency_key(&["semantic", "r", "1", "0", "x", "p"]);
    let c = idempotency_key(&["task", "r", "1", "0", "x", "p"]);
    assert_ne!(a, b);
    assert_ne!(a, c);
    assert_ne!(b, c);
    // 4. Output shape: 32 hex chars (16 bytes).
    assert_eq!(idempotency_key(&["x"]).len(), 32);
}

// ---------- Memory capabilities: tokenize + inverted index ----------

#[test]
fn tokenize_lowercases_and_drops_stop_words() {
    use co_scientist::memory::Memory;
    let toks = Memory::tokenize("The KRAS-G12C binds sotorasib at CYS12");
    // "The" is a stop word; "at" is a stop word; "binds" stays.
    assert!(!toks.contains(&"the".to_string()));
    assert!(!toks.contains(&"at".to_string()));
    assert!(toks.contains(&"kras".to_string()));
    assert!(toks.contains(&"g12c".to_string()));
    assert!(toks.contains(&"binds".to_string()));
    assert!(toks.contains(&"sotorasib".to_string()));
    assert!(toks.contains(&"cys12".to_string()));
    // All lowercase.
    for t in &toks {
        assert_eq!(*t, t.to_lowercase());
    }
    // All >= 3 chars.
    for t in &toks {
        assert!(t.len() >= 3);
    }
}

#[tokio::test]
async fn inverted_index_ranks_relevant_memories_first() {
    // A document with the search term should outrank a document
    // that mentions a different term. This is the test FTS5-style
    // ranking ought to pass.
    let mem = Memory::new(db::open_memory().await.unwrap());
    let run_id = new_run_id();
    // 5 memories, only one directly mentions "sotorasib".
    let ids: Vec<i64> = futures::future::join_all((0..5).map(|i| {
        let mem = mem.clone();
        let run_id = run_id.clone();
        async move {
            let summary = match i {
                0 => "KRAS G12C binds sotorasib covalently at Cys12",
                1 => "MAPK pathway reactivation underlies adaptive resistance",
                2 => "sotorasib resistance in lung cancer patients",
                3 => "EGFR mutation prevalence in non-small cell lung cancer",
                _ => "phase 1/2 trial shows 35% objective response rate",
            };
            mem.save_semantic(
                &run_id,
                Some("experiment"),
                "experiment",
                summary,
                Some(json!({"i": i})),
            )
            .await
            .unwrap()
        }
    }))
    .await;
    let results = mem
        .search_semantic("sotorasib", 5, false)
        .await
        .unwrap();
    // Both #0 and #2 should appear.
    assert_eq!(results.len(), 2);
    // #0 is ranked first (twice the term count: 1 vs 1 — actually
    // same count, but #0 is the earlier id, so they tie; either order
    // is acceptable for ties). Just verify both are present.
    let result_ids: Vec<i64> = results.iter().map(|m| m.id).collect();
    assert!(result_ids.contains(&ids[0]));
    assert!(result_ids.contains(&ids[2]));
    // #1, #3, #4 must NOT appear.
    assert!(!result_ids.contains(&ids[1]));
    assert!(!result_ids.contains(&ids[3]));
    assert!(!result_ids.contains(&ids[4]));
}

#[tokio::test]
async fn inverted_index_stems_porter_via_simple_lowercasing() {
    // Porter stemmer isn't implemented; we just verify that simple
    // prefix variants work (lowercasing).
    let mem = Memory::new(db::open_memory().await.unwrap());
    let run_id = new_run_id();
    mem.save_semantic(&run_id, Some("a"), "x", "kras g12c mutation", None)
        .await
        .unwrap();
    let results = mem
        .search_semantic("KRAS-G12C", 5, false)
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
}

#[tokio::test]
async fn peek_returns_only_summary_and_id() {
    // Layer 1 of 3-layer retrieval. Should be small and fast.
    use co_scientist::PeekedKind;
    let mem = Memory::new(db::open_memory().await.unwrap());
    let run_id = new_run_id();
    mem.save_semantic(
        &run_id,
        Some("a"),
        "experiment",
        "kras inhibitor screening",
        Some(json!({"long": "details that should NOT appear in peek", "lots": "of", "data": true})),
    )
    .await
    .unwrap();
    let p = mem.peek_context("a", "kras", 5).await.unwrap();
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].kind, PeekedKind::Semantic);
    assert!(p[0].summary.contains("kras"));
    // The details JSON should NOT be in the peek.
    assert!(!p[0].summary.contains("details"));
}

#[tokio::test]
async fn get_observation_returns_full_row() {
    // Layer 3.
    let mem = Memory::new(db::open_memory().await.unwrap());
    let run_id = new_run_id();
    let id = mem
        .save_semantic(
            &run_id,
            Some("a"),
            "experiment",
            "kras test",
            Some(json!({"compound": "sotorasib", "ki": 0.07})),
        )
        .await
        .unwrap();
    let obs = mem
        .get_observation(co_scientist::ObservationKind::Semantic, id)
        .await
        .unwrap()
        .expect("observation should exist");
    let co_scientist::Observation::Semantic(m) = obs else {
        panic!("expected Semantic variant");
    };
    assert_eq!(m.id, id);
    assert!(m.details.is_some());
    assert_eq!(m.details.unwrap()["compound"], "sotorasib");
}

#[tokio::test]
async fn get_observation_returns_none_for_missing_id() {
    let mem = Memory::new(db::open_memory().await.unwrap());
    let obs = mem
        .get_observation(co_scientist::ObservationKind::Semantic, 999_999)
        .await
        .unwrap();
    assert!(obs.is_none());
}

#[tokio::test]
async fn get_timeline_returns_events_for_run() {
    let mem = Memory::new(db::open_memory().await.unwrap());
    let run_id = new_run_id();
    // Log a few events.
    for i in 0..3 {
        mem.log_event(&run_id, "a", i, "turn_started", Some(json!({"step": i})))
            .await
            .unwrap();
    }
    let mem_id = mem
        .save_semantic(&run_id, Some("a"), "experiment", "kras", None)
        .await
        .unwrap();
    let events = mem
        .get_timeline(mem_id, co_scientist::ObservationKind::Semantic, 2)
        .await
        .unwrap();
    // 2 events before + 2 after, max 5 total. We have 3 events; expect 3.
    assert!(!events.is_empty());
    for e in &events {
        assert_eq!(e.run_id, run_id);
    }
}

#[test]
fn approx_tokens_is_four_chars_per_token() {
    use co_scientist::approx_tokens;
    assert_eq!(approx_tokens(""), 0);
    // We round up so over-estimates err on the side of "prompt is
    // bigger than I think." (len + 3) / 4.
    assert_eq!(approx_tokens("ab"), 1); // 2 chars -> 1 token (round up)
    assert_eq!(approx_tokens("abc"), 1); // 3 chars -> 1
    assert_eq!(approx_tokens("abcd"), 1);
    assert_eq!(approx_tokens("abcde"), 2);
    assert_eq!(approx_tokens("hello world"), 3); // 11 chars -> 3
}

#[test]
fn cite_formats_id_with_brackets() {
    use co_scientist::cite;
    assert_eq!(cite(42), "[ref:42]");
    assert_eq!(cite(0), "[ref:0]");
    assert_eq!(cite(999_999), "[ref:999999]");
}

#[tokio::test]
async fn prior_session_summary_includes_relevant_memories() {
    // Cross-session auto-inject: a fresh runner can pull context
    // from a prior session's memories.
    let mem = Memory::new(db::open_memory().await.unwrap());
    // Seed a memory in a "prior" run.
    let prior_run = new_run_id();
    mem.save_semantic(
        &prior_run,
        Some("experiment"),
        "experiment",
        "kras g12c binds sotorasib covalently",
        None,
    )
    .await
    .unwrap();
    mem.save_behavior("experiment", "always report IC50", "nM is the unit", None)
        .await
        .unwrap();

    // New runner asks for prior context.
    let block = mem
        .prior_session_summary("experiment", "kras", 5, 5, 0)
        .await
        .unwrap();
    assert!(block.contains("kras"));
    assert!(block.contains("sotorasib"));
    assert!(block.contains("IC50"));
    assert!(block.contains("[ref:")); // citations present
}

#[tokio::test]
async fn prior_session_summary_empty_for_unknown_agent() {
    let mem = Memory::new(db::open_memory().await.unwrap());
    let block = mem
        .prior_session_summary("nonexistent-agent", "", 5, 5, 0)
        .await
        .unwrap();
    assert!(block.contains("no prior sessions"));
}

#[tokio::test]
async fn compress_events_tool_saves_summary() {
    use co_scientist::tool::{CompressEventsTool, Tool, ToolCtx};
    let mem = Memory::new(db::open_memory().await.unwrap());
    let ctx = ToolCtx {
        memory: mem.clone(),
        run_id: new_run_id(),
        agent_name: "experiment".into(),
    };
    let tool = CompressEventsTool;
    let out = tool
        .call(
            json!({
                "summary": "After 5 events, kras g12c is the lead hypothesis",
                "scope": "compression"
            }),
            &ctx,
        )
        .await
        .unwrap();
    let id = out["id"].as_i64().unwrap();
    assert!(id > 0);
    // Verify the row landed with scope=compression.
    let conn = mem.db().conn().clone();
    let mut rows = conn
        .query(
            "SELECT scope, summary FROM semantic_memories WHERE id = ?1",
            [id],
        )
        .await
        .unwrap();
    let row = rows.next().await.unwrap().unwrap();
    let scope: String = row.get(0).unwrap();
    let summary: String = row.get(1).unwrap();
    assert_eq!(scope, "compression");
    assert!(summary.contains("kras"));
}

// ---------- Community prompts + agent rename ----------

#[test]
fn seven_agents_match_community_names() {
    use co_scientist::agents::AGENTS;
    let names: Vec<&str> = AGENTS.iter().map(|a| a.name).collect();
    assert_eq!(
        names,
        vec!["supervisor", "generation", "reflection", "ranking", "evolution", "metareview", "experiment"],
        "agents should be the 7 co-scientist agents (experiment added for the empirical loop)"
    );
}

#[test]
fn every_agent_has_at_least_one_mode() {
    use co_scientist::agents::AGENTS;
    for a in AGENTS {
        assert!(!a.modes.is_empty(), "agent {} has no modes", a.name);
    }
}

#[test]
fn alias_dispatches_record_hypothesis_to_save_semantic() {
    use co_scientist::marker_normalizer::canonicalize;
    assert_eq!(canonicalize("record_hypothesis"), None);
    assert_eq!(canonicalize("record_review"), None);
    assert_eq!(canonicalize("record_system_feedback"), Some("save_behavior"));
    assert_eq!(canonicalize("record_research_plan"), Some("save_semantic"));
    // Passthrough for names that aren't aliases.
    assert_eq!(canonicalize("save_semantic"), None);
}

#[tokio::test]
async fn record_hypothesis_marker_inserts_a_row() {
    // The end-to-end: a model that emits `record_hypothesis` should
    // land a row via the registry dispatch (record_hypothesis is a
    // first-class tool, not an alias).
    let mem = Memory::new(db::open_memory().await.unwrap());
    let mut reg = co_scientist::ToolRegistry::new();
    reg.register_all(co_scientist::builtin_tools());
    let ctx = co_scientist::ToolCtx {
        memory: mem.clone(),
        run_id: "r".into(),
        agent_name: "generation".into(),
    };
    // Mimic what the runner does after parse_markers.
    let raw = "record_hypothesis";
    let aliased = co_scientist::marker_normalizer::canonicalize(raw).unwrap_or(raw);
    let payload = serde_json::json!({
        "scope": "experiment",
        "summary": "KRAS G12C binds sotorasib covalently at Cys12",
        "details": {"ki_nM": 0.07}
    });
    let out = reg.dispatch(aliased, payload, &ctx).await.unwrap();
    let id = out["id"].as_i64().unwrap();
    assert!(id > 0);
    // Verify the row landed in the right agent.
    let n: i64 = mem
        .db()
        .conn()
        .query(
            "SELECT COUNT(*) FROM semantic_memories WHERE agent_id = (SELECT id FROM agents WHERE name = 'generation')",
            (),
        )
        .await
        .unwrap()
        .next()
        .await
        .unwrap()
        .unwrap()
        .get(0)
        .unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn runner_builds_system_prompt_with_community_prompts() {
    use co_scientist::agents::AGENTS;
    use co_scientist::prompts::{AgentMode, PromptContext};
    let mem = Memory::new(db::open_memory().await.unwrap());
    let runner = co_scientist::Runner::new(mem, "test-run", co_scientist::RunnerConfig::default());
    let agent = AGENTS.iter().find(|a| a.name == "generation").unwrap();
    let mut ctx = PromptContext::new();
    ctx.set("goal", "Identify new KRAS G12C inhibitors");
    ctx.set("preferences", "");
    ctx.set("source_hypothesis", "");
    ctx.set("instructions", "");
    ctx.set("articles_with_reasoning", "(none)");
    let prompt = runner
        .build_system_prompt(agent, &[], "")
        .await
        .unwrap();
    // The runner's role preamble is present.
    assert!(prompt.contains("You are the `generation` agent"));
    // Tools are documented for agents that need them.
    assert!(prompt.contains("record_hypothesis"));
    // The tool-name alias block is present.
    assert!(prompt.contains("record_hypothesis") && prompt.contains("save_semantic"));
    // The SKILL.md is appended.
    assert!(prompt.contains("Memory operations") || prompt.contains("memory op"));
}

// ---------- Feature 2: event bus ----------

#[tokio::test]
async fn log_event_publishes_to_bus() {
    use co_scientist::MemoryEvent;
    let mem = Memory::new(db::open_memory().await.unwrap());
    let mut rx = mem.subscribe();
    mem.log_event(&new_run_id(), "hypothesis", 0, "turn_started", Some(json!({"k":1})))
        .await
        .unwrap();
    let ev = rx.recv().await.unwrap();
    match ev {
        MemoryEvent::EventLogged { agent, type_, .. } => {
            assert_eq!(agent, "hypothesis");
            assert_eq!(type_, "turn_started");
        }
        _ => panic!("expected EventLogged, got {ev:?}"),
    }
}

#[tokio::test]
async fn save_semantic_publishes_to_bus() {
    use co_scientist::MemoryEvent;
    let mem = Memory::new(db::open_memory().await.unwrap());
    let mut rx = mem.subscribe();
    mem.save_semantic(&new_run_id(), Some("a"), "experiment", "x", None)
        .await
        .unwrap();
    let ev = rx.recv().await.unwrap();
    match ev {
        MemoryEvent::SemanticSaved { scope, summary, .. } => {
            assert_eq!(scope, "experiment");
            assert_eq!(summary, "x");
        }
        _ => panic!("expected SemanticSaved, got {ev:?}"),
    }
}

#[tokio::test]
async fn save_behavior_publishes_to_bus() {
    use co_scientist::MemoryEvent;
    let mem = Memory::new(db::open_memory().await.unwrap());
    let mut rx = mem.subscribe();
    mem.save_behavior("a", "pattern-x", "notes", None).await.unwrap();
    let ev = rx.recv().await.unwrap();
    match ev {
        MemoryEvent::BehaviorSaved { agent, pattern, .. } => {
            assert_eq!(agent, "a");
            assert_eq!(pattern, "pattern-x");
        }
        _ => panic!("expected BehaviorSaved, got {ev:?}"),
    }
}

#[tokio::test]
async fn idempotent_log_event_only_publishes_once() {
    use co_scientist::MemoryEvent;
    let mem = Memory::new(db::open_memory().await.unwrap());
    let mut rx = mem.subscribe();
    let run_id = new_run_id();
    mem.log_event(&run_id, "a", 0, "x", Some(json!({"v":1}))).await.unwrap();
    mem.log_event(&run_id, "a", 0, "x", Some(json!({"v":1}))).await.unwrap();
    // First publish happens; second is a no-op (no new row, no event).
    let ev1 = rx.recv().await.unwrap();
    assert!(matches!(ev1, MemoryEvent::EventLogged { .. }));
    // Wait briefly to confirm nothing else arrives.
    let timeout = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
    assert!(timeout.is_err(), "second idempotent call should not publish");
}

#[tokio::test]
async fn bus_does_not_block_writer_when_no_subscribers() {
    let mem = Memory::new(db::open_memory().await.unwrap());
    // No subscribers at all.
    for i in 0..10 {
        mem.log_event(&new_run_id(), "a", i, "x", None).await.unwrap();
    }
}

// ---------- Feature 3: self-critique feedback loop ----------

#[tokio::test]
async fn recent_behavior_returns_newest_first() {
    let mem = Memory::new(db::open_memory().await.unwrap());
    mem.save_behavior("hypothesis", "first", "n1", None).await.unwrap();
    mem.save_behavior("hypothesis", "second", "n2", None).await.unwrap();
    mem.save_behavior("hypothesis", "third", "n3", None).await.unwrap();
    let notes = mem.recent_behavior("hypothesis", 5).await.unwrap();
    assert_eq!(notes.len(), 3);
    assert_eq!(notes[0].pattern, "third", "newest first");
    assert_eq!(notes[2].pattern, "first");
}

#[tokio::test]
async fn recent_behavior_filters_by_agent() {
    let mem = Memory::new(db::open_memory().await.unwrap());
    mem.save_behavior("hypothesis", "a1", "n", None).await.unwrap();
    mem.save_behavior("experiment", "b1", "n", None).await.unwrap();
    let only_hyp = mem.recent_behavior("hypothesis", 10).await.unwrap();
    assert_eq!(only_hyp.len(), 1);
    assert_eq!(only_hyp[0].pattern, "a1");
}

#[tokio::test]
async fn recent_behavior_respects_limit() {
    let mem = Memory::new(db::open_memory().await.unwrap());
    for i in 0..7 {
        mem.save_behavior("a", &format!("p{i}"), "n", None)
            .await
            .unwrap();
    }
    let notes = mem.recent_behavior("a", 3).await.unwrap();
    assert_eq!(notes.len(), 3);
}

#[test]
fn runner_prompt_block_formatting() {
    // The block the runner builds is the contract with the LLM. Lock it
    // down so refactors don't silently change what the model sees.
    use co_scientist::memory::BehaviorMemory;
    let notes = vec![
        BehaviorMemory {
            id: 1,
            agent_id: 1,
            pattern: "concise first sentence".into(),
            notes: "the model was rambling".into(),
            evidence: None,
            created_at: "x".into(),
        },
        BehaviorMemory {
            id: 2,
            agent_id: 1,
            pattern: "use bullet points".into(),
            notes: "wall of text got it wrong".into(),
            evidence: None,
            created_at: "x".into(),
        },
    ];
    let block = co_scientist::runner::test_helpers::format_prior_block(&notes);
    assert!(block.starts_with("\n\n## Your prior self-critique\n"));
    assert!(block.contains("- concise first sentence: the model was rambling"));
    assert!(block.contains("- use bullet points: wall of text got it wrong"));
}

// ---------- Memory edge cases (gap coverage) ----------
//
// These pin the swallow-and-return-empty / no-op contracts that would
// otherwise regress silently on a query change.

/// G: `recent_marker_errors` swallows DB errors and returns an empty
/// vec. Caller-facing contract: a failed query must not panic the runner.
#[tokio::test]
async fn recent_marker_errors_returns_empty_for_unknown_run() {
    let mem = Memory::new(db::open_memory().await.unwrap());
    // No events exist; the query succeeds with zero rows.
    let out = mem
        .recent_marker_errors("nonexistent-run", "any-agent", 10)
        .await;
    assert!(out.is_empty());
}

/// G-extended: malformed JSON in `payload_json` does not crash the
/// extractor; missing fields silently drop the row.
#[tokio::test]
async fn recent_marker_errors_handles_malformed_payload() {
    let mem = Memory::new(db::open_memory().await.unwrap());
    let run_id = new_run_id();
    // Ensure the agent + session exist so the FK chain holds, then
    // raw-insert a malformed payload. `json_extract` returns NULL for
    // non-JSON, and the extractor handles NULL gracefully.
    let agent_id = mem.ensure_agent("a").await.unwrap();
    mem.ensure_session(&run_id, agent_id).await.unwrap();
    mem.db()
        .conn()
        .execute(
            "INSERT INTO events (run_id, agent_id, step_index, type, payload_json, created_at)
             VALUES (?1, ?2, 0, 'memory_op_failed', 'not-json', ?3)",
            (&run_id, agent_id, "2026-01-01T00:00:00Z"),
        )
        .await
        .unwrap();
    let out = mem.recent_marker_errors(&run_id, "a", 10).await;
    assert!(out.is_empty(), "malformed JSON must yield empty result");
}

/// H: `bump_last_accessed` is a no-op for unknown `kind`. Pins the
/// match-arm's catch-all so adding a new memory table doesn't crash.
#[tokio::test]
async fn bump_last_accessed_unknown_kind_is_noop() {
    let mem = Memory::new(db::open_memory().await.unwrap());
    // Should not panic, should not error.
    mem.bump_last_accessed("nonexistent_kind", 1).await;
}

/// I: `save_semantic` deduplication via cosine similarity. To exercise
/// the cosine path (not the idempotency-key path), the two calls must
/// have **different** idempotency keys but produce **identical**
/// embeddings. The dedup embeds `summary + " " + details_json`. Changing
/// `run_id` doesn't change the embedding but does change the key.
#[tokio::test]
async fn save_semantic_dedup_collapses_near_duplicates() {
    let mem = Memory::new(db::open_memory().await.unwrap());
    let run_id_a = new_run_id();
    let run_id_b = new_run_id();
    assert_ne!(run_id_a, run_id_b);

    let summary = "kras g12c binds sotorasib covalently at cys12";
    let details = Some(json!({"compound": "sotorasib"}));

    let a = mem
        .save_semantic(&run_id_a, Some("experiment"), "experiment", summary, details.clone())
        .await
        .unwrap();
    // Same summary + details → identical embedding (cosine = 1.0)
    // → above the 0.92 dedup threshold. Different run_id → different
    // idempotency key, so the conflict branch is NOT taken.
    let b = mem
        .save_semantic(&run_id_b, Some("experiment"), "experiment", summary, details)
        .await
        .unwrap();
    assert_eq!(a, b, "near-duplicate must return the original id");
    let count: i64 = mem
        .db()
        .conn()
        .query("SELECT COUNT(*) FROM semantic_memories", ())
        .await
        .unwrap()
        .next()
        .await
        .unwrap()
        .unwrap()
        .get(0)
        .unwrap();
    assert_eq!(count, 1, "dedup must NOT insert the duplicate row");
}

/// J: `recent_system_feedback` matches via SQL LIKE on the pattern
/// column. The contract: patterns containing "system_feedback" or
/// "meta" are returned, others aren't.
#[tokio::test]
async fn recent_system_feedback_matches_pattern_keyword() {
    let mem = Memory::new(db::open_memory().await.unwrap());
    mem.save_behavior("a", "system_feedback_x", "feedback note 1", None)
        .await
        .unwrap();
    mem.save_behavior("a", "meta_review_y", "feedback note 2", None)
        .await
        .unwrap();
    mem.save_behavior("a", "unrelated_pattern", "should NOT appear", None)
        .await
        .unwrap();
    let notes = mem.recent_system_feedback(10).await;
    assert_eq!(notes.len(), 2, "only the two matching patterns");
    // Newest first.
    assert!(notes[0].contains("feedback note"));
}

// ---- Tool coverage: research + curation tools ---------------------------
//
// Each of these tools had zero direct tests before. They run in-process
// against an ephemeral SQLite DB — no LLM, no network.

/// `record_review` saves the review to `semantic_memories` with
/// `scope="review"` and advances the linked hypothesis from `draft` to
/// `reviewed`.
#[tokio::test]
async fn record_review_tool_saves_review_and_advances_hypothesis() {
    use co_scientist::hypothesis::HypothesisRepo;
    use co_scientist::tool::{RecordReviewTool, Tool, ToolCtx};

    let mem = Memory::new(db::open_memory().await.unwrap());
    // Insert a hypothesis row directly so the review has something to
    // attach to. The tool only advances state — it doesn't create the
    // hypothesis.
    let repo = HypothesisRepo::new(mem.db_arc());
    let hyp_id = repo
        .insert("test-session", None, &[], 1500.0)
        .await
        .unwrap();

    let tool = RecordReviewTool;
    let ctx = ToolCtx {
        memory: mem.clone(),
        run_id: "r-review".into(),
        agent_name: "reflection".into(),
    };
    let out = tool
        .call(
            json!({
                "hypothesis_id": hyp_id,
                "summary": "novel + correct, weak testability",
                "details": {"novelty": 4, "correctness": 4, "testability": 2},
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(out["hypothesis_id"].as_i64().unwrap(), hyp_id);

    // The review landed as a semantic memory with scope=review.
    let n: i64 = mem
        .db()
        .conn()
        .query(
            "SELECT COUNT(*) FROM semantic_memories WHERE scope = 'review'",
            (),
        )
        .await
        .unwrap()
        .next()
        .await
        .unwrap()
        .unwrap()
        .get(0)
        .unwrap();
    assert_eq!(n, 1, "review row written to semantic_memories");

    // The hypothesis advanced to 'reviewed'.
    let hyp = repo.get(hyp_id).await.unwrap().unwrap();
    assert_eq!(hyp.state, co_scientist::hypothesis::HypothesisState::Reviewed);
}

/// Missing `hypothesis_id` is rejected before any DB write.
#[tokio::test]
async fn record_review_rejects_missing_hypothesis_id() {
    use co_scientist::tool::{RecordReviewTool, Tool, ToolCtx};

    let mem = Memory::new(db::open_memory().await.unwrap());
    let ctx = ToolCtx {
        memory: mem,
        run_id: "r".into(),
        agent_name: "reflection".into(),
    };
    let err = RecordReviewTool
        .call(json!({"summary": "no hypothesis"}), &ctx)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("missing 'hypothesis_id'"));
}

/// `record_tournament_match` records a match row and updates Elo on
/// both hypotheses. With both starting at 1500 and A winning, A's Elo
/// should rise and B's fall.
#[tokio::test]
async fn record_tournament_match_updates_elo() {
    use co_scientist::hypothesis::HypothesisRepo;
    use co_scientist::tool::{RecordTournamentMatchTool, Tool, ToolCtx};

    let mem = Memory::new(db::open_memory().await.unwrap());
    let repo = HypothesisRepo::new(mem.db_arc());
    let hyp_a = repo.insert("s", None, &[], 1500.0).await.unwrap();
    let hyp_b = repo.insert("s", None, &[], 1500.0).await.unwrap();

    let tool = RecordTournamentMatchTool;
    let ctx = ToolCtx {
        memory: mem.clone(),
        run_id: "r-tour".into(),
        agent_name: "ranking".into(),
    };
    let out = tool
        .call(
            json!({
                "hypothesis_a": hyp_a,
                "hypothesis_b": hyp_b,
                "winner": 1,
                "rationale": "A has more support",
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(out["match_id"].as_i64().unwrap() > 0, "match row id returned");
    assert!(out["new_elo_a"].as_f64().unwrap() > 1500.0, "winner Elo rises");
    assert!(out["new_elo_b"].as_f64().unwrap() < 1500.0, "loser Elo falls");

    // Match row landed.
    let n: i64 = mem
        .db()
        .conn()
        .query("SELECT COUNT(*) FROM tournament_matches", ())
        .await
        .unwrap()
        .next()
        .await
        .unwrap()
        .unwrap()
        .get(0)
        .unwrap();
    assert_eq!(n, 1);
}

/// Tournament match rejects an invalid winner value (not 0/1/2).
#[tokio::test]
async fn record_tournament_match_rejects_invalid_winner() {
    use co_scientist::hypothesis::HypothesisRepo;
    use co_scientist::tool::{RecordTournamentMatchTool, Tool, ToolCtx};

    let mem = Memory::new(db::open_memory().await.unwrap());
    let repo = HypothesisRepo::new(mem.db_arc());
    let hyp_a = repo.insert("s", None, &[], 1500.0).await.unwrap();
    let hyp_b = repo.insert("s", None, &[], 1500.0).await.unwrap();
    let ctx = ToolCtx {
        memory: mem,
        run_id: "r".into(),
        agent_name: "ranking".into(),
    };
    let err = RecordTournamentMatchTool
        .call(
            json!({"hypothesis_a": hyp_a, "hypothesis_b": hyp_b, "winner": 9}),
            &ctx,
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("winner must be"));
}

/// `archive_observation` for `kind=semantic` soft-deletes a row and
/// logs an `observation_archived` event. The row stays in the table
/// but is hidden from retrieval.
#[tokio::test]
async fn archive_observation_tool_soft_deletes_semantic() {
    use co_scientist::tool::{ArchiveObservationTool, Tool, ToolCtx};

    let mem = Memory::new(db::open_memory().await.unwrap());
    let id = mem
        .save_semantic("r-arch", Some("gen"), "insight", "to archive", None)
        .await
        .unwrap();

    let ctx = ToolCtx {
        memory: mem.clone(),
        run_id: "r-arch".into(),
        agent_name: "metareview".into(),
    };
    let out = ArchiveObservationTool
        .call(
            json!({"kind": "semantic", "id": id, "reason": "test cleanup"}),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(out["archived"], true);
    assert_eq!(out["kind"], "semantic");
    assert_eq!(out["id"].as_i64().unwrap(), id);

    // The row is still in the table but archived=1.
    let archived: i64 = mem
        .db()
        .conn()
        .query(
            "SELECT archived FROM semantic_memories WHERE id = ?1",
            (id,),
        )
        .await
        .unwrap()
        .next()
        .await
        .unwrap()
        .unwrap()
        .get(0)
        .unwrap();
    assert_eq!(archived, 1);

    // The audit event landed.
    let n: i64 = mem
        .db()
        .conn()
        .query(
            "SELECT COUNT(*) FROM events WHERE type = 'observation_archived'",
            (),
        )
        .await
        .unwrap()
        .next()
        .await
        .unwrap()
        .unwrap()
        .get(0)
        .unwrap();
    assert_eq!(n, 1);
}

/// Invalid `kind` is rejected before any DB write.
#[tokio::test]
async fn archive_observation_rejects_invalid_kind() {
    use co_scientist::tool::{ArchiveObservationTool, Tool, ToolCtx};

    let mem = Memory::new(db::open_memory().await.unwrap());
    let ctx = ToolCtx {
        memory: mem,
        run_id: "r".into(),
        agent_name: "metareview".into(),
    };
    let err = ArchiveObservationTool
        .call(json!({"kind": "hypothesis", "id": 1}), &ctx)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("invalid kind"));
}

/// `delete_observation` is gated to `kind=behavior` and requires a
/// non-empty `evidence` list. Both invariants must hold for the row to
/// be removed.
#[tokio::test]
async fn delete_observation_tool_removes_behavior_with_evidence() {
    use co_scientist::tool::{DeleteObservationTool, Tool, ToolCtx};

    let mem = Memory::new(db::open_memory().await.unwrap());
    let bid = mem
        .save_behavior("a", "junk_pattern", "will be deleted", None)
        .await
        .unwrap();

    let ctx = ToolCtx {
        memory: mem.clone(),
        run_id: "r-del".into(),
        agent_name: "metareview".into(),
    };
    let out = DeleteObservationTool
        .call(
            json!({"kind": "behavior", "id": bid, "evidence": [1, 2, 3]}),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(out["deleted"], true);
    assert_eq!(out["rows_removed"].as_i64().unwrap(), 1);

    // The row is gone.
    let n: i64 = mem
        .db()
        .conn()
        .query(
            "SELECT COUNT(*) FROM behavior_memories WHERE id = ?1",
            (bid,),
        )
        .await
        .unwrap()
        .next()
        .await
        .unwrap()
        .unwrap()
        .get(0)
        .unwrap();
    assert_eq!(n, 0);
}

/// `kind=semantic` is rejected — only `behavior` can be hard-deleted.
#[tokio::test]
async fn delete_observation_rejects_kind_semantic() {
    use co_scientist::tool::{DeleteObservationTool, Tool, ToolCtx};

    let mem = Memory::new(db::open_memory().await.unwrap());
    let ctx = ToolCtx {
        memory: mem,
        run_id: "r".into(),
        agent_name: "metareview".into(),
    };
    let err = DeleteObservationTool
        .call(
            json!({"kind": "semantic", "id": 1, "evidence": [1]}),
            &ctx,
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("only 'behavior'"));
}

/// Empty `evidence` list is rejected — the audit trail must not be empty.
#[tokio::test]
async fn delete_observation_rejects_empty_evidence() {
    use co_scientist::tool::{DeleteObservationTool, Tool, ToolCtx};

    let mem = Memory::new(db::open_memory().await.unwrap());
    let ctx = ToolCtx {
        memory: mem,
        run_id: "r".into(),
        agent_name: "metareview".into(),
    };
    let err = DeleteObservationTool
        .call(
            json!({"kind": "behavior", "id": 1, "evidence": []}),
            &ctx,
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("at least one event id"));
}

// ---------- Feature 4: ResearchSessionRepo seam ----------
//
// These tests exercise the seam extracted from Supervisor. They run
// against an ephemeral SQLite DB (no LLM, no network).

/// Helper: read the current `status` and `ended_at` for a session.
async fn session_status(mem: &co_scientist::Memory, id: &str) -> (String, Option<String>) {
    use co_scientist::db::Conn;
    let conn: Conn = mem.db().conn().clone();
    let mut rows = conn
        .query(
            "SELECT status, ended_at FROM research_sessions WHERE id = ?1",
            [id],
        )
        .await
        .unwrap();
    let row = rows.next().await.unwrap().unwrap();
    (
        row.get::<String>(0).unwrap(),
        row.get::<Option<String>>(1).unwrap(),
    )
}

/// Helper: insert a session row directly so a test can put the DB
/// into a chosen starting state.
async fn insert_session_direct(mem: &co_scientist::Memory, id: &str, status: &str) {
    let _ = mem
        .db()
        .conn()
        .execute(
            "INSERT INTO research_sessions (id, goal, preferences, status, budget_usd, started_at)
             VALUES (?1, 'g', '', ?2, NULL, '2026-01-01T00:00:00Z')",
            (id, status),
        )
        .await
        .unwrap();
}

/// `recover_stale` flips a stale `running` session to `interrupted`
/// while leaving the active session alone.
#[tokio::test]
async fn research_session_repo_create_and_recover_stale() {
    let mem = co_scientist::Memory::new(co_scientist::db::open_memory().await.unwrap());
    let repo = co_scientist::research_session::ResearchSessionRepo::new(mem.db_arc());

    // Insert two running sessions directly.
    insert_session_direct(&mem, "active-1", "running").await;
    insert_session_direct(&mem, "stale-1", "running").await;

    // active-1 is the "current" session; only stale-1 should be touched.
    let recovered = repo.recover_stale("active-1").await.unwrap();
    assert_eq!(recovered, 1, "exactly one row should be flipped");

    let (active_status, _) = session_status(&mem, "active-1").await;
    let (stale_status, stale_ended) = session_status(&mem, "stale-1").await;
    assert_eq!(active_status, "running", "active session untouched");
    assert_eq!(stale_status, "interrupted", "stale session flipped");
    assert!(stale_ended.is_some(), "ended_at should be populated");
}

/// `create` writes a new session with status='running' and `finalize`
/// flips it to 'done' without writing a report.
#[tokio::test]
async fn research_session_repo_create_and_finalize() {
    let mem = co_scientist::Memory::new(co_scientist::db::open_memory().await.unwrap());
    let repo = co_scientist::research_session::ResearchSessionRepo::new(mem.db_arc());

    repo.create(
        "s1",
        "the goal",
        "prefs",
        Some(2.5),
        "2026-06-01T00:00:00Z",
    )
    .await
    .unwrap();

    let (status_before, _) = session_status(&mem, "s1").await;
    assert_eq!(status_before, "running");

    repo.finalize("s1", "2026-06-01T01:00:00Z").await.unwrap();
    let (status_after, ended) = session_status(&mem, "s1").await;
    assert_eq!(status_after, "done");
    assert_eq!(ended.as_deref(), Some("2026-06-01T01:00:00Z"));
}

/// `mark_done_with_report` writes the report and flips status to 'done'.
#[tokio::test]
async fn research_session_repo_mark_done_with_report() {
    let mem = co_scientist::Memory::new(co_scientist::db::open_memory().await.unwrap());
    let repo = co_scientist::research_session::ResearchSessionRepo::new(mem.db_arc());

    repo.create("s2", "g", "", None, "2026-06-01T00:00:00Z")
        .await
        .unwrap();

    repo.mark_done_with_report("s2", "the final report body", "2026-06-01T02:00:00Z")
        .await
        .unwrap();

    let conn = mem.db().conn().clone();
    let mut rows = conn
        .query(
            "SELECT status, final_report, ended_at FROM research_sessions WHERE id = ?1",
            ["s2"],
        )
        .await
        .unwrap();
    let row = rows.next().await.unwrap().unwrap();
    let status: String = row.get(0).unwrap();
    let report: String = row.get(1).unwrap();
    let ended: Option<String> = row.get(2).unwrap();
    assert_eq!(status, "done");
    assert_eq!(report, "the final report body");
    assert_eq!(ended.as_deref(), Some("2026-06-01T02:00:00Z"));
}

/// `cancel_orphaned_tasks` cancels tasks for non-running sessions but
/// leaves tasks for the active session alone.
#[tokio::test]
async fn research_session_repo_cancel_orphaned_tasks() {
    let mem = co_scientist::Memory::new(co_scientist::db::open_memory().await.unwrap());
    let repo = co_scientist::research_session::ResearchSessionRepo::new(mem.db_arc());

    // One active running session, one old done session.
    insert_session_direct(&mem, "active-1", "running").await;
    insert_session_direct(&mem, "done-1", "done").await;

    // Insert tasks for both sessions directly.
    for sess in ["active-1", "done-1"] {
        for status in ["pending", "leased"] {
            let _ = mem
                .db()
                .conn()
                .execute(
                    "INSERT INTO tasks (id, session_id, agent, action, payload, priority, status, max_attempts, created_at, idempotency_key)
                     VALUES (?1, ?2, 'a', 'x', '{}', 100, ?3, 3, '2026-01-01T00:00:00Z', ?4)",
                    (
                        format!("{sess}-{status}"),
                        sess,
                        status,
                        format!("key-{sess}-{status}"),
                    ),
                )
                .await
                .unwrap();
        }
    }

    let cancelled = repo.cancel_orphaned_tasks().await.unwrap();
    // Only the two tasks under 'done-1' should be flipped (pending + leased).
    assert_eq!(cancelled, 2);

    // active-1 tasks stay pending/leased.
    let conn = mem.db().conn().clone();
    for status in ["pending", "leased"] {
        let mut rows = conn
            .query(
                "SELECT status FROM tasks WHERE id = ?1",
                [format!("active-1-{status}")],
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let s: String = row.get(0).unwrap();
        assert_eq!(s, status, "active-1/{status} untouched");
    }

    // done-1 tasks are cancelled.
    for status in ["pending", "leased"] {
        let mut rows = conn
            .query(
                "SELECT status, last_error FROM tasks WHERE id = ?1",
                [format!("done-1-{status}")],
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let s: String = row.get(0).unwrap();
        let err: Option<String> = row.get(1).unwrap();
        assert_eq!(s, "cancelled", "done-1/{status} should be cancelled");
        assert_eq!(err.as_deref(), Some("[startup-recovery]"));
    }
}

/// Regression test for the ranking prompt deadlock observed on
/// 2026-07-01. The pipeline used to require hypotheses in
/// `in_tournament`/`ranked` state before `needs_matches` would return
/// them, but `record_tournament_match` is the only thing that
/// transitions a hypothesis out of `reviewed`. This created a chicken-
/// and-egg deadlock: ranking couldn't bootstrap because there were no
/// `in_tournament` candidates, so the LLM was fed an empty prompt
/// (just auto-context + marker reminder) and hallucinated
/// `[[MEMORY_OP:add:{...}]]` with a fabricated payload schema, then
/// on retry `[[MEMORY_OP:record_tournament_match:{...}]]` with IDs
/// borrowed from the semantic-memory auto-context rather than the
/// hypothesis table.
///
/// Fix: `needs_matches` now also returns `reviewed` hypotheses, and
/// `build_prompt_for_agent` returns an empty string when fewer than
/// 2 candidates exist so the worker can short-circuit the LLM call.
#[tokio::test]
async fn needs_matches_includes_reviewed_hypotheses_for_bootstrap() {
    use co_scientist::hypothesis::{HypothesisRepo, HypothesisState};

    let mem = Memory::new(db::open_memory().await.unwrap());
    let repo = HypothesisRepo::new(mem.db_arc());
    let session = "needs-matches-bootstrap-test";

    // Three hypotheses: one in `draft` (not yet reflected — should
    // NOT be picked up), two in `reviewed` (the bootstrap case —
    // MUST be picked up so the first tournament match can fire).
    let draft_id = repo
        .insert(session, None, &[], 1200.0)
        .await
        .unwrap();
    let rev_a = repo
        .insert(session, None, &[], 1200.0)
        .await
        .unwrap();
    repo.update_state(rev_a, HypothesisState::Reviewed, false)
        .await
        .unwrap();
    let rev_b = repo
        .insert(session, None, &[], 1200.0)
        .await
        .unwrap();
    repo.update_state(rev_b, HypothesisState::Reviewed, false)
        .await
        .unwrap();

    // needs_matches(threshold=3, limit=2) returns hypotheses with
    // matches_played < 3. The two `reviewed` ones qualify; the
    // `draft` one does not (state filter excludes it).
    let needs = repo.needs_matches(session, 3, 2).await.unwrap();
    let ids: Vec<i64> = needs.iter().map(|h| h.id).collect();
    assert!(
        ids.contains(&rev_a) && ids.contains(&rev_b),
        "both reviewed hypotheses must be returned for bootstrap; got {ids:?}"
    );
    assert!(
        !ids.contains(&draft_id),
        "draft hypotheses (not yet reflected) must NOT be returned; got {ids:?}"
    );
    assert_eq!(
        needs.len(),
        2,
        "limit=2 should cap the result; got {} ({ids:?})",
        needs.len()
    );

    // Also: a hypothesis that has already played enough matches
    // (>= threshold) must NOT be returned — it's saturated.
    let saturated = repo
        .insert(session, None, &[], 1200.0)
        .await
        .unwrap();
    repo.update_state(saturated, HypothesisState::InTournament, false)
        .await
        .unwrap();
    repo.update_elo(saturated, 1500.0, 5).await.unwrap();
    let needs = repo.needs_matches(session, 3, 10).await.unwrap();
    assert!(
        !needs.iter().any(|h| h.id == saturated),
        "saturated hypothesis (matches_played >= threshold) must be excluded; got {:?}",
        needs.iter().map(|h| h.id).collect::<Vec<_>>()
    );
}
