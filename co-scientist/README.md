# co-scientist

External agent memory layer over Ante. Marker-based skill protocol, local SQLite, tournament-based hypothesis ranking.

## Read these in order

1. **[`SKILL.md`](SKILL.md)** — the LLM-facing skill document. Loaded into the system prompt. Defines the marker format (`[[MEMORY_OP:<tool>:{...json...}]]`) and the three memory operations the model can call: `save_semantic`, `save_behavior`, `get_context` (and the token-efficient `peek_context` / `get_timeline` / `get_observation` trio).
2. **[`CONTEXT.md`](CONTEXT.md)** — domain glossary. Defines `Marker`, `MarkerNormalizer`, `MemoryEvent`, `Tool`, `Run`, `Tournament`, `ConsolidationService`, and the `recent marker errors` self-correction loop. Use these names when discussing architecture; the codebase treats them as first-class concepts.
3. **[`IMPROVEMENT_PLAN.md`](IMPROVEMENT_PLAN.md)** — known issues and the 8-item roadmap. Source-of-truth for "what's next."

## What's in this crate

```
src/
├── runner.rs              # the agent loop — turns, marker dispatch, retry
├── registry.rs            # ToolRegistry: typed dispatch + per-agent allowlists
├── supervisor.rs          # orchestrator: idle injection + termination
├── tools/                 # 12 tools split by category
│   ├── mod.rs             # trait + ctx + builtin_tools()
│   ├── memory.rs          # 7 memory/retrieval tools
│   ├── research.rs        # 3 structured research tools
│   └── curation.rs        # 2 destructive curation tools
├── memory/                # persistence split by table
│   ├── semantic.rs        # save_semantic, find_near_duplicate, term_index
│   ├── behavior.rs        # save_behavior, search_behavior
│   ├── context.rs         # get_context, prior_session_summary
│   ├── events.rs          # log_event
│   ├── helpers.rs         # tokenize, idempotency_key
│   └── types.rs           # shared row types
├── marker_normalizer.rs   # pure seam: alias + scope + summary derivation
├── research_session.rs    # ResearchSessionRepo: owns all session SQL
├── policies.rs            # IdlePolicy + TerminationPolicy (pure structs)
├── llm_query.rs           # retry helpers: is_transient_error, jitter
├── skill.rs               # parse_markers + the LLM-friendly marker parser
├── skill_loader.rs        # discover/load skill bundles from disk
├── hypothesis.rs          # Hypothesis model + HypothesisRepo
├── tournament.rs          # pairwise ranking + Elo updates
├── promotion.rs           # ConsolidationService (background daemon)
├── queue.rs               # durable TaskQueue with leases
├── bus.rs                 # in-process EventBus (MemoryEvent variants)
├── embeddings.rs          # optional embedding API (off by default)
├── claude_cli.rs          # Claude CLI subprocess wrapper
└── claude.rs, agent.rs, ... (smaller pieces)
```

## Build and test

```bash
# Build
cargo build -p co-scientist

# All tests (134 lib + 63 integration, no LLM required)
cargo test -p co-scientist --lib
cargo test -p co-scientist --test integration --features test-helpers

# Lint
cargo clippy -p co-scientist --lib
```

Tests run against an ephemeral SQLite database (via `db::open_memory()`). Each test is fully self-contained — no shared state, no LLM subprocess, no network. The full suite finishes in well under a second.

## Marker format (cheat sheet)

The LLM emits one or more of these per turn:

```
[[MEMORY_OP:save_semantic:{"scope":"experiment","summary":"<one line>","details":{...}}}]]
[[MEMORY_OP:save_behavior:{"pattern":"<short name>","notes":"<observation>","evidence":[1,2,3]}]]
[[MEMORY_OP:get_context:{"query":"<question>","limit":5}]]
[[MEMORY_OP:peek_context:{"query":"<q>","limit":10}]]
[[MEMORY_OP:get_timeline:{"observation_id":42,"kind":"semantic","around":3}]]
[[MEMORY_OP:get_observation:{"kind":"semantic","id":42}]]
[[MEMORY_OP:record_hypothesis:{"summary":"H1",...}]]
[[MEMORY_OP:record_review:{"hypothesis_id":1,"summary":"<verdict>",...}]]
[[MEMORY_OP:record_tournament_match:{"hypothesis_a":1,"hypothesis_b":2,"winner":1,"rationale":"..."}]]
[[MEMORY_OP:noop:{}]]
```

Community aliases (auto-rewritten by `MarkerNormalizer`):

- `record_research_plan` → `save_semantic` with `scope="plan"` auto-filled
- `record_system_feedback` → `save_behavior`

The parser (`src/skill.rs`) is LLM-tolerant: extra `]` chars are consumed, lowercase prefixes are accepted, whitespace after the colon is allowed, malformed markers are warned-and-skipped (so one bad marker doesn't swallow the rest of the response).

## License

Same as the parent super-workspace. See the root [`README.md`](../README.md).