# Agents

A super-workspace combining two Rust crates:

| Crate | Role |
|---|---|
| [`ante-preview/`](ante-preview/) | AI agent runtime (vendored snapshot) |
| [`co-scientist/`](co-scientist/) | External agent memory layer built on top of Ante |

## What's here

**Ante** is the agent loop — it handles model calls, tool registration, and the conversation flow. It's the vendored upstream snapshot; if you want to track upstream changes, this directory should become a submodule or be re-vendored.

**Co-scientist** is the layer above it. It treats the LLM as a turn primitive and bolts on three things Ante doesn't provide natively:

1. **Marker-based skill protocol** — the LLM emits structured `[[MEMORY_OP:...]]` markers in its response text. Co-scientist parses them and dispatches to a typed tool registry. See [`co-scientist/SKILL.md`](co-scientist/SKILL.md) for the marker format and the prompt-side contract.
2. **Local memory layer** — SQLite-backed, ephemeral by default, no network. Three memory kinds: `semantic` (insights), `behavior` (patterns), and `events` (turn trace). Tournament-based hypothesis ranking via Elo updates.
3. **Reflection + consolidation** — a Supervisor daemon that decides when to spawn reflection agents, when to terminate, and how to recover from prior crashed sessions.

The full domain glossary lives in [`co-scientist/CONTEXT.md`](co-scientist/CONTEXT.md) — start there to understand the moving pieces.

## Quick start

```bash
# Build the default members (Ante crates + co-scientist)
cargo build

# Run the test suite (134 lib + 63 integration tests, all green, no LLM needed)
cargo test -p co-scientist --lib
cargo test -p co-scientist --test integration --features test-helpers

# Lint
cargo clippy -p co-scientist --lib
```

Co-scientist requires Node.js ≥ 22 only if you wire the `claude` CLI for live runs. Tests don't need it.

## Architecture

The interesting seams:

- **`MarkerNormalizer`** (`co-scientist/src/marker_normalizer.rs`) — pure function that turns `(raw_op, payload)` into `(canonical_op, payload)`. Handles community alias rewrites and prompt-convention defaults (e.g. `record_research_plan` implies `scope="plan"`). Where `runner.rs` used to bake in three concerns (alias + scope inference + summary derivation), this is the seam.
- **`ToolRegistry`** (`co-scientist/src/registry.rs`) — typed dispatch with per-agent allowlists. Tools live in `co-scientist/src/tools/{memory,research,curation}.rs`.
- **`ResearchSessionRepo`** (`co-scientist/src/research_session.rs`) — owns all `research_sessions` SQL, mirroring `HypothesisRepo`.
- **`IdlePolicy` / `TerminationPolicy`** (`co-scientist/src/policies.rs`) — pure structs the Supervisor consults to decide when to inject reflection tasks and when to terminate the run.
- **`llm_query`** (`co-scientist/src/llm_query.rs`) — retry/transient-error classification for LLM calls. Helpers extracted; the retry loop stays in `runner.rs` because `ClaudeHandle` is private there.

A full architecture review identified six deepening candidates plus a few follow-ups. All five of the high-value ones have been landed. See the git log for the per-candidate commits.

## Repo layout

```
.
├── Cargo.toml              # super-workspace manifest
├── Cargo.lock
├── .cargo/config.toml      # build flags (turso-sqlite math, tokio_unstable)
├── ante-preview/           # AI agent runtime (vendored)
├── co-scientist/           # memory layer (the work)
│   ├── Cargo.toml
│   ├── SKILL.md            # LLM-facing skill (system prompt content)
│   ├── CONTEXT.md          # domain glossary
│   ├── IMPROVEMENT_PLAN.md # known issues + roadmap
│   ├── RUN_UNTIL_PAPER_PLAN.md
│   ├── prompts/            # 14 community-prompt templates
│   ├── src/
│   │   ├── runner.rs       # agent loop
│   │   ├── registry.rs     # tool dispatch
│   │   ├── supervisor.rs   # orchestrator
│   │   ├── tools/          # split by category
│   │   ├── research_session.rs
│   │   ├── policies.rs
│   │   ├── llm_query.rs
│   │   ├── marker_normalizer.rs
│   │   └── memory/         # persistence split by table
│   └── tests/integration.rs
└── reports/                # 7 research reports on memory-system designs
```

## Notes

This is not Ante upstream. `ante-preview/` is a snapshot — the upstream history is on GitHub at `https://github.com/DietrichGebert/ante` (or similar; check the directory's own README for the canonical source). If you want to track upstream changes, either re-vendor periodically or convert this directory to a git submodule.

The `co_scientist.db` and any `*.db.tmp` files are gitignored — they contain local run data and never belong in the repo.