# Agents

A super-workspace combining two Rust crates:

| Crate | Role |
|---|---|
| [`ante-preview/`](ante-preview/) | AI agent runtime (vendored snapshot) |
| [`co-scientist/`](co-scientist/) | External agent memory layer built on top of Ante |

## What's here

**Ante** is the agent loop — it handles model calls, tool registration, and the conversation flow. It's the vendored upstream snapshot; if you want to track upstream changes, this directory should become a submodule or be re-vendored.

**Co-scientist** is the layer above it. It treats the LLM as a turn primitive and bolts on five things Ante doesn't provide natively:

1. **Marker-based skill protocol** — the LLM emits structured `[[MEMORY_OP:...]]` markers in its response text. Co-scientist parses them and dispatches to a typed tool registry. See [`co-scientist/SKILL.md`](co-scientist/SKILL.md) for the marker format and the prompt-side contract.
2. **Local memory layer** — SQLite-backed, ephemeral by default, no network. Three memory kinds: `semantic` (insights), `behavior` (patterns), and `events` (turn trace). Tournament-based hypothesis ranking via Elo updates.
3. **Reflection + consolidation** — a Supervisor daemon that decides when to spawn reflection agents, when to terminate, and how to recover from prior crashed sessions.
4. **Empirical loop** — an `experiment` agent that designs, executes, and evaluates Python experiments that test hypotheses. Results feed back into the tournament via a `reflection_on_result` pass.
5. **Per-session Runner cache + marker-failure aggregator** — long supervisor sessions reuse one `Runner` per `(session, agent)` pair so the Claude subprocess connect cost amortizes; a bus subscriber aggregates `MarkerFailed` events into periodic `FailureStats` reports for downstream reflection passes.

The full domain glossary lives in [`co-scientist/CONTEXT.md`](co-scientist/CONTEXT.md) — start there to understand the moving pieces.

## Quick start

```bash
# Build the default members (Ante crates + co-scientist)
cargo build

# Run the test suite (228 lib + 63 integration tests, all green, no LLM needed)
cargo test -p co-scientist --lib
cargo test -p co-scientist --test integration --features test-helpers

# Lint
cargo clippy -p co-scientist --lib
```

Co-scientist requires Node.js ≥ 22 only if you wire the `claude` CLI for live runs. Tests don't need it.

## Architecture

The interesting seams:

- **`MarkerNormalizer`** (`co-scientist/src/marker_normalizer.rs`) — pure function that turns `(raw_op, payload)` into `(canonical_op, payload)`. Handles community alias rewrites and prompt-convention defaults (e.g. `record_research_plan` implies `scope="plan"`). The validation that should propagate up as `MemoryEvent::MarkerFailed` (e.g. missing `summary` on `save_semantic`) lives here, not in the runner.
- **`ToolRegistry`** (`co-scientist/src/registry.rs`) — typed dispatch with per-agent allowlists. Tools live in `co-scientist/src/tools/{memory,research,curation,experiment}.rs`. `PromptToolTable` (`co_scientist/src/prompt_allowlist.rs`) cross-checks the embedded prompt templates against the allowlists at build time.
- **`Runner`** (`co-scientist/src/runner.rs`) — the agent loop. Owns a `Memory` handle, a `Prompts` registry, and a `ClaudeHandle`. The turn path lives in `run_turn_inner(TurnStrategy)`; `turn()` and `turn_stream()` are 4-line wrappers that pick the strategy. `TurnPhase::FirstTurn / Subsequent` gates the personality + skills preamble blocks. `PromptContextCache` (single-slot, keyed by `step_index`) caches the four DB-derived blocks (`prior_behavior`, `prior_session`, `marker_errors`, `get_context`) so the second and later turns of a session don't refetch.
- **`SessionRunners`** (`co-scientist/src/run_agent.rs`) — per-session Runner cache keyed on `(session_id, agent_name)`. `RunAgentTool` consults it on every worker task; 50 generation tasks in one session share one `Runner`.
- **`FOLLOW_UP_SPECS`** (`co-scientist/src/run_agent.rs`) — static DAG table for the research pipeline. Each `FollowUpSpec { from_agent, from_mode, next_agent, next_mode, priority, requires_hypothesis }` is one edge; `spec_for(agent, mode)` resolves the downstream dispatch. New stages add one row, not one match arm.
- **`EventBus` + `run_failure_aggregator`** (`co-scientist/src/bus.rs`) — `tokio::sync::broadcast` channel carrying `MemoryEvent` variants. `MarkerFailed` events (published by `Runner::dispatch_marker` on registry errors) are aggregated by `run_failure_aggregator` and re-published as periodic `FailureStats { window, top, total }` events so downstream reflection passes can mine which tools fail most per agent.
- **`ResearchSessionRepo`** (`co-scientist/src/research_session.rs`) — owns all `research_sessions` SQL, mirroring `HypothesisRepo`.
- **`IdlePolicy` / `TerminationPolicy`** (`co-scientist/src/policies.rs`) — pure structs the Supervisor consults to decide when to inject reflection tasks and when to terminate the run. Stable, no DB, no async — testable without spinning up the orchestrator.
- **`llm_query`** (`co-scientist/src/llm_query.rs`) — retry/transient-error classification for LLM calls. Helpers extracted; the retry loop stays in `runner.rs` because `ClaudeHandle` is private there.

## Recent work

A full architecture review identified six deepening candidates plus a few follow-ups. All six are landed; recent commits:

- `Deepen runner + bus + run_agent seams; add 48 edge-case tests` — C1–C6 (collapse turn/turn_stream, `TurnPhase`, `PromptContextCache`, `MarkerFailed` aggregator, `SessionRunners`, `FOLLOW_UP_SPECS`) plus an edge-case test pass that pins every failure boundary so the loop never breaks on a malformed input.
- `Add experiment agent and 3-stage empirical loop` — seventh agent, design/execute/evaluate tools, post-experiment reflection stage.

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
│   ├── prompts/            # 18 community-prompt templates
│   ├── src/
│   │   ├── runner.rs       # agent loop (TurnPhase, PromptContextCache)
│   │   ├── run_agent.rs    # SessionRunners, FOLLOW_UP_SPECS
│   │   ├── bus.rs          # EventBus, run_failure_aggregator
│   │   ├── registry.rs     # tool dispatch
│   │   ├── supervisor.rs   # orchestrator
│   │   ├── tools/          # split by category
│   │   │   ├── memory.rs
│   │   │   ├── research.rs
│   │   │   ├── curation.rs
│   │   │   └── experiment.rs
│   │   ├── research_session.rs
│   │   ├── policies.rs
│   │   ├── llm_query.rs
│   │   ├── marker_normalizer.rs
│   │   ├── prompt_allowlist.rs
│   │   └── memory/         # persistence split by table
│   └── tests/integration.rs
└── reports/                # 7 research reports on memory-system designs
```

## Notes

This is not Ante upstream. `ante-preview/` is a snapshot — the upstream history is on GitHub at `https://github.com/DietrichGebert/ante` (or similar; check the directory's own README for the canonical source). If you want to track upstream changes, either re-vendor periodically or convert this directory to a git submodule.

The `co_scientist.db` and any `*.db.tmp` files are gitignored — they contain local run data and never belong in the repo.
