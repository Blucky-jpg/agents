# Co-Scientist Improvement Plan — Lessons from 7 Memory-System Investigations

**Date**: 2026-06-26
**Source analysis**: `/home/blucky/Agents/reports/` (claude-mem, continuous-learning-v2, ai-agent-os, claudelicious, claude-self-learning-loop, memory-production-systems-survey, MiMo-Code)
**Target project**: `/home/blucky/Agents/co-scientist/`
**Status**: Plan (no code changes yet)

---

## §0 Executive summary

We read all 7 reports and cross-checked each "interesting mechanism" against the co-scientist codebase. **Most of what these systems invented co-scientist already has** — inverted index, dual-write bus, durable queue with leases, idempotent inserts, per-agent allowlist, event-driven consolidation daemon. The real wins are smaller and more specific.

**8 changes total** across 2 tiers. Tier 1 is 4 small, high-leverage fixes. Tier 2 is 4 medium items, gated on Tier 1 outcomes.

**Total estimated code surface**: ~120 LOC of new code, 3 schema column additions, 0 architectural changes. No new external dependencies.

---

## §1 Tier 1 — high value density (do first)

### §1.1 Stop silently producing junk `summary` rows

- **Source**: `claude-self-learning-loop.md` §4.4, `claudelicious.md` §3.3
- **Problem**: `runner.rs:518-525` falls back to `format!("({}) auto-summary from {:?}", raw_name, keys)` when the LLM forgets the `summary` field. That writes `(record_hypothesis) auto-summary from ["details", "scope"]` into `semantic_memories`. The `term_index` then indexes junk tokens and surfaces them in `search_semantic` and `get_context`. This is the exact failure mode the claude-self-learning-loop report warns about: *"the only defense is the dedup check at SKILL.md:112-121, which catches near-duplicates but not low-quality originals."*
- **Why it's a win**: makes the existing `recent_marker_errors` self-correction loop (`runner.rs:256-271`, `memory.rs:218-248`) actually fire. Today the fallback eats the error before it reaches `log_event("memory_op_failed")` at `runner.rs:564`.
- **Cost**: tiny (one `unwrap_or_else` replacement, ~5 LOC).
- **Risk**: more dispatches fail loudly. Net positive because the marker-errors block surfaces them in the next turn's system prompt, giving the LLM a chance to self-correct.
- **Files**: `src/runner.rs:518-525` only.

**Concrete edit**:
```rust
// runner.rs:518-525 — replace this:
.unwrap_or_else(|| {
    let keys: Vec<String> = obj.keys().cloned().collect();
    format!("({}) auto-summary from {:?}", raw_name, keys)
});

// with this:
return Err(anyhow::anyhow!(
    "save_semantic: missing 'summary' and no recognized alternative (objective/verdict/statement)"
));
```

The `Err` propagates through `dispatch_marker`, hits the existing `memory_op_failed` log_event at `runner.rs:564`, and `recent_marker_errors` will surface it on the next turn's system prompt via the existing injection at `runner.rs:264-270`.

---

### §1.2 Wire the dead `MarkerFailed` bus variant + add `marker_outcomes` table

- **Source**: `claude-self-learning-loop.md` §3, `claudelicious.md` §3.1
- **Problem**: `bus.rs:49` defines `MemoryEvent::MarkerFailed { agent, op, error }` but it's **never published**. Meanwhile `runner.rs:559-568` logs `memory_op_failed` events as write-only telemetry — no one subscribes structurally. Both reports call this out as the *meta-signal* (which tools the LLM keeps misusing) being more valuable than the call outcome itself.
- **Why it's a win**: enables a future reflection pass to mine which tools fail most often per agent. Today that data exists in `events` but you have to grep for it.
- **Cost**: small (~15 LOC + one column migration).
- **Risk**: extra write per failed dispatch (rare path; not on hot loop). Possible PII leak via `payload_keys` → mitigation: record *key names only*, not values.
- **Files**: `src/runner.rs:550-571`, `src/bus.rs:14-54`, `src/db.rs:248`.

**Concrete steps**:

1. `src/bus.rs` — `MarkerFailed` already exists at line 49. No change needed.

2. `src/db.rs` — add column via `try_add_column` pattern (line 278-298). Add after the existing `try_add_column` block (line 255):
   ```rust
   try_add_column(&conn, "events", "marker_op_outcome", "TEXT").await?;
   ```
   Stores JSON `{op, agent, error_class, payload_keys, success}` so post-hoc queries can `WHERE json_extract(marker_op_outcome, '$.success') = 0`.

3. `src/runner.rs` — at the dispatch site (line 534-571), split the success and failure paths:
   ```rust
   Ok(_out) => {
       self.memory.log_event(
           &self.run_id, agent.name, self.step_index, "memory_op",
           Some(json!({"op": raw_name, "aliased_to": tool_name, "outcome": "success"})),
       ).await?;
       Ok(())
   }
   Err(e) => {
       debug!(op = raw_name, error = %e, "tool dispatch failed");
       self.bus.publish(crate::bus::MemoryEvent::MarkerFailed {
           agent: agent.name.to_string(),
           op: raw_name.clone(),
           error: e.to_string(),
       });
       self.memory.log_event(
           &self.run_id, agent.name, self.step_index, "memory_op_failed",
           Some(json!({
               "op": raw_name,
               "aliased_to": tool_name,
               "error": e.to_string(),
               "payload_keys": payload.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()),
           })),
       ).await.ok();
       Err(e)
   }
   ```

---

### §1.3 Confidence decay + last-accessed tracking

- **Source**: `continuous-learning-v2.md` §9 #2 + §10 #3, `claude-self-learning-loop.md` §7.2-b
- **Problem**: `semantic_memories.importance` exists (`db.rs:129`, default 1.0) but is never read for ranking or decay. `promotion.rs:188-280` only uses it to pick cluster representatives. Old, irrelevant rows accumulate in `term_index` forever — same anti-pattern claudelicious §1.5 calls "carried x21."
- **Why it's a win**: makes the system actually age its memories instead of growing monotonically. Without decay, retrieval quality degrades over long sessions.
- **Cost**: small (~30 LOC + 2 column adds + 1 promotion phase).
- **Risk**: if `importance` is currently used to *prioritize* results (it isn't — `get_context` doesn't sort by it; verify by grep before merging), changing semantics could surprise callers. Mitigation: decay applies only to rows **older than 30 days AND not accessed in 30 days**.
- **Files**: `src/db.rs:248`, `src/memory.rs` (search_semantic + peek_semantic), `src/promotion.rs:66-91`.

**Concrete steps**:

1. `src/db.rs` — add columns after line 255:
   ```rust
   try_add_column(&conn, "semantic_memories", "last_accessed_at", "TEXT").await?;
   try_add_column(&conn, "behavior_memories", "last_accessed_at", "TEXT").await?;
   ```

2. `src/memory.rs` — add a bump helper (around line 1500):
   ```rust
   pub async fn bump_last_accessed(&self, kind: &str, id: i64) -> Result<(), MemoryError> {
       let table = match kind {
           "semantic" => "semantic_memories",
           "behavior" => "behavior_memories",
           _ => return Ok(()),
       };
       let sql = format!(
           "UPDATE {table} SET last_accessed_at = ?1 WHERE id = ?2",
           table = table
       );
       self.db.conn().execute(&sql, (Utc::now().to_rfc3339(), id)).await?;
       Ok(())
   }
   ```
   Call it from `search_semantic` (`memory.rs:1104`) and `peek_semantic` (`memory.rs:1311`) after results are returned.

3. `src/promotion.rs` — add Phase 4 to `run_consolidation` (line 66-91), before the return:
   ```rust
   // Phase 4: decay unused memories.
   let decayed = decay_unused_memories(memory, 30, 0.95).await?;
   stats.decayed = decayed; // add `decayed: usize` to ConsolidationStats
   ```
   Implementation:
   ```rust
   async fn decay_unused_memories(memory: &Memory, days: i64, factor: f64) -> Result<usize> {
       let cutoff = (Utc::now() - chrono::Duration::days(days)).to_rfc3339();
       let mut rows = memory.db().conn().query(
           "SELECT id, importance FROM semantic_memories
            WHERE archived = 0
              AND last_accessed_at IS NOT NULL
              AND last_accessed_at < ?1",
           (cutoff,),
       ).await?;
       let mut ids = Vec::new();
       while let Some(row) = rows.next().await? {
           ids.push((row.get::<i64>(0)?, row.get::<f64>(1)?));
       }
       let mut count = 0;
       for (id, importance) in ids {
           let new_imp = importance * factor;
           if new_imp < 0.1 {
               memory.archive_semantic(id).await?;
           } else {
               memory.db().conn().execute(
                   "UPDATE semantic_memories SET importance = ?1 WHERE id = ?2",
                   (new_imp, id),
               ).await?;
           }
           count += 1;
       }
       Ok(count)
   }
   ```

---

### §1.4 Tighten `save_behavior` schema with structured fields

- **Source**: `claude-self-learning-loop.md` §2.1, `memory-production-systems-survey.md` §7.1
- **Problem**: `SaveBehaviorTool` accepts arbitrary `notes` prose. The report's claim is explicit: *"the embeddings will be worse if the Skill allows verbose fields."* `hash_bag` is even more verbosity-sensitive than a real embedder because it counts token repeats without semantic understanding.
- **Why it's a win**: makes behavior memories usable as retrieval hits, not just per-agent self-critique. Also enables A-MEM-style neighbor rewriting as a future direction (per `memory-production-systems-survey.md` §7.5) once the schema is structured.
- **Cost**: small (~20 LOC). Backward compatible: existing `notes` continues to work; new fields activate only when present.
- **Risk**: existing `behavior_memories.pattern/notes` rows lack the new fields. Mitigation: keep all three new fields optional in the schema; old rows render via `notes` only.
- **Files**: `src/tool.rs:128-148`, optionally `src/embeddings.rs:26-47` (better embedder input).

**Concrete edit** to `src/tool.rs` `SaveBehaviorTool::input_schema` (line 128-148):
```rust
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
```

Update `SaveBehaviorTool::call` (line 149-165) to prefer the structured fields:
```rust
let body = match (args.get("error_summary"), args.get("cause"), args.get("fix")) {
    (Some(e), Some(c), Some(f)) => format!("{} | cause: {} | fix: {}", e.as_str().unwrap_or(""), c.as_str().unwrap_or(""), f.as_str().unwrap_or("")),
    _ => notes.to_string(),
};
let id = ctx.memory.save_behavior(&ctx.agent_name, pattern, &body, evidence).await?;
Ok(serde_json::json!({ "id": id }))
```

---

## §2 Tier 2 — gated on Tier 1 outcomes

### §2.1 Add `archive_observation` / `delete_observation` tools

- **Source**: `claude-self-learning-loop.md` §5.4
- **Precondition**: §1.1 must be live, otherwise deleting observations just means new junk appears faster.
- **Cost**: medium (~40 LOC + `behavior_memories.archived` column).
- **Files**: `src/tool.rs:687-700`, `src/memory.rs:1478` (already implemented for semantic), `src/db.rs:248` (add `archived` column to `behavior_memories`), `src/registry.rs:158-188` (allowlist for metareview + supervisor only).

**Concrete tools**:
- `ArchiveObservationTool` — calls `memory.archive_semantic(id)` for `kind=semantic`, new `memory.archive_behavior(id)` for `kind=behavior`.
- `DeleteObservationTool` — hard-delete for behavior (rare, requires `evidence` payload), soft-delete (archive) for semantic.

**Risk**: lets agents delete their own memories. Mitigation: require `evidence: [event_id, ...]` array (audit trail) and rate-limit per session.

---

### §2.2 Promote `prior_session_summary` to per-turn refresh

- **Source**: `claude-mem.md` §8, `MiMo-Code.md` §7
- **Precondition**: none, but benefits from §1.3 decay (otherwise stale memories dominate).
- **Problem**: `runner.rs:294` calls `prior_session_summary(agent_name, "", 5, 3)` only on `connect()` (first time a Runner is built). For long sessions, cross-session context is stale. Also no `max_tokens` cap — output can blow up.
- **Cost**: small (~10 LOC + 1 config field).
- **Files**: `src/runner.rs:42-72` (add `prior_semantic_limit` to `RunnerConfig`), `src/memory.rs:1342` (add `max_chars` arg to `prior_session_summary`), `src/runner.rs:288-297` (move call to per-turn).

**Concrete steps**:
1. `src/memory.rs:1342` signature change:
   ```rust
   pub async fn prior_session_summary(
       &self,
       agent_name: &str,
       query: &str,
       semantic_limit: usize,
       behavior_limit: usize,
       max_chars: usize, // NEW — 0 = unlimited
   ) -> Result<String, MemoryError>
   ```
   Truncate the rendered string at `max_chars` before returning.
2. `src/runner.rs:42-72` add to `RunnerConfig`:
   ```rust
   pub prior_semantic_limit: usize, // default 10
   pub prior_behavior_limit: usize, // default 5
   pub prior_max_chars: usize,      // default 4000
   ```
3. `src/runner.rs:288-297` move into `Runner::turn` step 0 block (line 356-375), call alongside `get_context`.

---

### §2.3 Council-style parallel reflection fan-out

- **Source**: `claudelicious.md` §4.4
- **Precondition**: none, but A/B test against sequential baseline before committing.
- **Idea**: `reflection` agent has 3 modes (`agents.rs:29-33`) running sequentially via `decide_next_steps`. Replace with: enqueue all 3, run in parallel via the existing worker pool, then enqueue a synthesis pass that sees all three outputs.
- **Cost**: medium (~60 LOC + new `council` flag on `RunAgentTool`).
- **Risk**: doubles LLM cost for reflection turns. **Don't ship speculatively** — measure first.
- **Files**: `src/tool.rs` (new `RunReflectionFanOutTool` or `council` flag), `src/supervisor.rs:299-384` (replace reflection branch), `src/agents.rs:29-33` (add `synthesizer` mode to REFLECTION_MODES).

**Measurement requirement**: instrument `n_reflections_synthesized` and `reflection_quality_score` before merging. A/B compare against sequential baseline on at least 2 research sessions.

---

### §2.4 `confidence_score` + `access_count` columns

- **Source**: `continuous-learning-v2.md` §4
- **Precondition**: probably redundant with §1.3. **Pick one** before doing both.
- **Idea**: replace the unused `importance` with an actual feedback loop: retrieval bumps `confidence_score += 0.05`, time decays it.
- **Risk**: rich-get-richer failure mode (popular memories get reinforced, useful-but-rare ones decay). Mitigation: keep `importance` for explicit cluster-rep selection, use `confidence_score` only for decay decisions.
- **Files**: same as §1.3 — pick one approach.

**Recommendation**: skip unless §1.3 proves insufficient. §1.3 is simpler and solves the same problem.

---

## §3 Explicitly skipped — already done in co-scientist

Each item below is a "novel mechanism" from a report that co-scientist already has. Cited so reviewers don't re-suggest them.

- **Storage-layer dedup via UNIQUE + ON CONFLICT DO NOTHING** — claude-mem §13, survey §6. Your `idempotency_key` UNIQUE indexes (`db.rs:261-264`) + `ON CONFLICT(idempotency_key) DO NOTHING RETURNING id` (`memory.rs:439-440, 515-519, 619-622`) match. **Yours is actually stronger**: hashes kind + run_id + agent_id + full payload, vs claude-mem's `UNIQUE(memory_session_id, content_hash)` which only hashes title+summary.
- **Hybrid markdown + SQLite (operational data in DB, narrative in MD)** — ai-agent-os §6.4. N/A; co-scientist is SQLite-only by design (Turso file at `./co_scientist.db`). No markdown surface to duplicate.
- **FTS5 + vector hybrid retrieval** — claude-mem §7, MiMo-Code §2. co-scientist has its own hand-rolled inverted index (`term_index` at `db.rs:179`) + `hash_bag`/`fastembed` (`embeddings.rs:26-126`). Not FTS5, but functionally equivalent — Turso's libSQL build doesn't ship FTS5 by default (noted at `db.rs:174-178`).
- **Per-agent tool allowlist** — claudelicious §2.3, ai-agent-os §1. `default_allowlist` (`registry.rs:157-188`) + `ToolRegistry.for_agent` (`registry.rs:60-69`).
- **Schema-versioned migrations** — claude-mem §13. `try_add_column` (`db.rs:278-298`) + idempotent `IF NOT EXISTS` indexes (`db.rs:144-186`).
- **Lease-protected queue with reclaim** — claude-mem §13. `queue.rs:191-232` lease/claim + `queue.rs:383-419` reclaim + `worker.rs:77-125` shutdown-aware loop.
- **3-tier execution architecture** — ai-agent-os §2. `supervisor.rs:74-243` (Tier 1) + `worker.rs` + `queue.rs` (Tier 2) + `promotion.rs:332-411` `ConsolidationService` (Tier 3). All three present, just not labeled.
- **Subagent isolation in fresh session** — MiMo-Code §6.1. `Runner` creates fresh `claude` CLI subprocess per Runner instance (`runner.rs:286-344`); queue tracks per-worker.
- **Capture-then-process learning loop** — ai-agent-os §3.2, claudelicious §2.2. Capture via `runner.rs:425-432` + `runner.rs:478-572` + `memory.rs:472-659`. Process via `promotion.rs:188-280` cluster-and-archive.
- **1-sentence distillation in template** — cl-s-l-loop §2.1. `save_semantic` schema (`tool.rs:78-96`) already requires `summary` as one-sentence. The 3-field *error/cause/fix* discipline is the missing piece (see §1.4).
- **Idempotency on every write** — claude-mem §13. Every table has `idempotency_key`; every write uses `ON CONFLICT DO NOTHING RETURNING id`. Stronger than claude-mem's approach.
- **Dual-write to SQLite + event bus** — claude-mem §4, MiMo-Code §1. `bus.rs:14-54` + `self.bus.publish(...)` in `memory.rs:457-465, 535-540, 635-639`. Supervisor subscribes at `supervisor.rs:170`.
- **Fail-open on hook errors** — cl-s-l-loop §7.1-d, claudelicious §3.5. `worker.rs:147-198` catch_unwind + `runner.rs:687-703` `is_transient_error`.
- **Background consolidation daemon** — clv2 §10 #4, claude-mem §4, MiMo-Code §10. `promotion.rs:332-411` `ConsolidationService::run` subscribes to bus, runs every `min_interval` (default 60s). This is the load-bearing piece that clv2 + claude-mem explicitly admit they lack.
- **Marker-error self-correction injection** — `runner.rs:256-271` reads `memory_op_failed` events via `recent_marker_errors` (`memory.rs:218-248`) and surfaces them in next turn's system prompt. **Wired correctly; just no errors reach it (see §1.1).**

---

## §4 Explicitly skipped — not a fit

- **claude-mem's `SessionMessageBuffer` (in-RAM queue + transcript JSONL)** — claude-mem §6. Wrong direction; you already have durable SQLite queue.
- **claude-mem's Chroma + uvx subprocess + per-project collections** — claude-mem §7. Inline embeddings + cosine dedup covers your scale.
- **claude-mem's `mode` system (CLAUDE_MEM_MODE=code)** — claude-mem §11. Your `scope` enum (`db.rs:125` + `tool.rs:82-85`) already gives you 8 scopes: `experiment | insight | result | question | review | hypothesis | compression | plan`.
- **claude-mem's tier routing (fast/smart/simple models)** — claude-mem §11. Research loop where every turn is high-stakes; picking cheap model for some turns is the wrong trade. Skip unless cost becomes a real constraint.
- **ai-agent-os's "3-tier agent autonomy graduation"** (Tier 1 → 2 → 3 with human approval) — ai-agent-os §2. Co-scientist is a fully-automated research loop; adding human review inverts the design.
- **ai-agent-os's `/claudeception` skill-extraction engine** — ai-agent-os §4. Static 14-template prompt catalog (`prompts.rs:130-156`, `include_str!`). No runtime skill extraction needed.
- **ai-agent-os's `content-guard.sh` (banned-pattern PostToolUse hook)** — ai-agent-os §5.3. Writes go through `tool.rs` + `memory.rs`, not filesystem edits. Different risk model.
- **clv2's instinct format with confidence in YAML** — clv2 §3.4. Your `importance` column encodes the same. YAML files require a workflow co-scientist doesn't have.
- **claude-self-learning-loop's stop-hook pattern** — cl-s-l-loop §3.3. Co-scientist runs as a Rust binary, not inside Claude Code. No hook surface. The equivalent (`worker.rs:147-198` panic-safe dispatch + retry/dead-letter) is already done.
- **claude-self-learning-loop's Slack approval gate** — cl-s-l-loop §6. Headless research loop, no human-in-the-loop surface.
- **claudelicious's `dream` skill (weekly cron consolidation)** — claudelicious §7.1. Your `promotion.rs` does this continuously (default 60s). Continuous is strictly better for a long-running research process.
- **claudelicious's `mneme` / `continuum` / `Pulsar`** — claudelicious §2.1. Opinionated single-user Node implementations. Their *concepts* map to existing primitives (mneme ≈ `memory.search_*`, continuum ≈ `runner.build_system_prompt`, Pulsar ≈ `supervisor.run`'s main loop) but the code is not portable.
- **MiMo-Code's `history_fts` for verbatim recall** — MiMo-Code §11. Your `events.rendered_prompt` + `events.raw_response` (`db.rs:254-255`, written at `runner.rs:389-397, 446-454`) already provide verbatim recall without a separate table.
- **MiMo-Code's MEMORY.md size cap (200 lines / 10 KB)** — MiMo-Code §14. Co-scientist has no markdown memory surface; analog is `events` row count, unbounded. Worth a soft cap on `events` per session — but separate recommendation, not directly from MiMo-Code.
- **A-MEM's auto-rewriting-neighbors** — survey §4, §7.5. Co-scientist's `promotion.rs:188-280` does the *opposite*: cluster + archive + don't rewrite. Adding neighbor rewriting would require versioning the report itself admits A-MEM lacks. **Not a fit today.**

---

## §5 Execution order

```
1. §1.1   5 min   biggest immediate quality win
2. §1.2   30 min  enables self-correction loop to fire
3. §1.4   30 min  makes behavior memories useful as embeddings
4. §1.3   1 hr    makes system age its memories
5. measure before §2.x
6. §2.1   1 hr    gated on §1.1 being live
7. §2.2   30 min  independent, can run any time after §1.3
8. §2.3   ???     A/B test required before committing
9. §2.4   skip    redundant with §1.3
```

---

## §6 Verification plan

For each Tier 1 change:

1. **Build**: `cargo build` from `/home/blucky/Agents/co-scientist/` must succeed.
2. **Tests**: `cargo test --features test-helpers` — integration tests at `tests/integration.rs` cover the marker parse path.
3. **Migration safety**: every `try_add_column` is idempotent (`db.rs:278-298`); existing DBs migrate on first open.
4. **Behavioral check**:
   - §1.1: emit a malformed marker → expect a `memory_op_failed` event with the new error message; expect the next turn's system prompt to contain it under "Previous turn marker errors."
   - §1.2: trigger the same failure → expect `MarkerFailed` event on the bus + `marker_op_outcome` JSON column populated.
   - §1.3: insert a row with `last_accessed_at` = 30+ days ago → run `promotion::run_consolidation` → expect `importance` decayed and row archived if < 0.1.
   - §1.4: call `save_behavior` with `error_summary/cause/fix` → expect `notes`-equivalent body stored + retrievable via `search_behavior`.

---

## §7 Open questions for the user

1. **§2.3 council fan-out**: do you want to commit the measurement instrumentation first, or skip this entirely? It doubles LLM cost on reflection turns.
2. **§2.4 vs §1.3**: pick one. §1.3 (decay by access time) is simpler. §2.4 (decay by confidence score with retrieval feedback) is more dynamic but risks rich-get-richer.
3. **Turso FTS5**: would adding the SQLite FTS5 extension (requires rebuilding libSQL) unlock a cleaner retrieval path than the hand-rolled `term_index`? If yes, that's a larger refactor — separate plan.

---

## §8 Source files referenced

Co-scientist:
- `src/db.rs` (schema, migrations)
- `src/memory.rs` (Memory API, retrieval, tokenization)
- `src/skill.rs` (LLM marker parser)
- `src/tool.rs` (10 built-in Tool impls)
- `src/registry.rs` (ToolRegistry, default_allowlist)
- `src/runner.rs` (Runner turn loop)
- `src/supervisor.rs` (orchestrator)
- `src/worker.rs` (queue dispatcher)
- `src/promotion.rs` (ConsolidationService)
- `src/embeddings.rs` (hash_bag + fastembed)
- `src/bus.rs` (tokio broadcast event bus)
- `src/agents.rs` (6 agent definitions)

Reports (all under `/home/blucky/Agents/reports/`):
- `claude-mem-memory-system.md` (584 lines)
- `continuous-learning-v2-memory-report.md` (574 lines)
- `investigation-ai-agent-os.md` (421 lines)
- `investigation-claudelicious.md` (406 lines)
- `investigation-claude-self-learning-loop.md` (464 lines)
- `memory-production-systems-survey.md` (639 lines)
- `MiMo-Code-memory-system.md` (620 lines)