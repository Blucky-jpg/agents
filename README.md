# Agents

An automated research lab. Two Rust crates:

| Crate | Role |
|---|---|
| [`ante-preview/`](ante-preview/) | AI agent runtime (vendored snapshot of upstream) |
| [`co-scientist/`](co-scientist/) | Memory layer + 7-agent research pipeline + empirical loop |

## The project

This is a local, offline-capable multi-agent system that turns a research goal into ranked, evidence-backed hypotheses. The pipeline:

```
goal → supervisor → generation → reflection → ranking → evolution → reflection_on_result → metareview
                              ↘ experiment (design → execute → evaluate) → reflection_on_result
```

Seven agents run in sequence, communicating through a SQLite-backed memory layer and a durable `TaskQueue`. The `experiment` agent closes the empirical loop — it writes Python code, runs it in a sandbox, evaluates metrics, and feeds results back through reflection so Elo scores reflect actual evidence, not just LLM debate.

## Crates

### [`ante-preview/`](ante-preview/)

The agent loop runtime. Vendored snapshot — not a submodule, not tracking upstream. If you need an upstream update, re-vendor or convert to a git submodule. See its own README for the canonical upstream URL.

### [`co-scientist/`](co-scientist/) — the work

The memory layer and 7-agent pipeline. Key entry points:

- **`Runner`** (`src/runner.rs`, ~2000 LOC) — the agent loop. Owns a `Memory` handle, a `Prompts` registry, and a `ClaudeHandle`. Each turn runs through `run_turn_inner(TurnStrategy)`, with `turn()` and `turn_stream()` as 4-line wrappers. The system prompt is composed of 8 blocks (role, personality, agent skills, SKILL.md, prior self-critique, prior session summary, tools block, marker errors).
- **`SessionRunners`** (`src/run_agent.rs`) — per-session Runner cache keyed on `(session_id, agent_name)`. Long supervisor sessions reuse one Runner per agent so the Claude subprocess connect cost amortizes.
- **`FOLLOW_UP_SPECS`** (`src/run_agent.rs`) — static DAG table for the research pipeline. Each `FollowUpSpec` is one edge (e.g. `generation → reflection_review`, `experiment_design → experiment_execute → experiment_evaluate → reflection_on_result`). Adding a stage is a one-row edit.
- **`EventBus` + `run_failure_aggregator`** (`src/bus.rs`) — in-process `tokio::sync::broadcast` channel. `MarkerFailed` events published by `dispatch_marker` are aggregated and re-published as `FailureStats` for downstream reflection passes.
- **`PromptContextCache`** (`src/runner.rs`) — single-slot cache keyed by `step_index` for the four DB-derived blocks (`prior_behavior`, `prior_session`, `marker_errors`, `get_context`). Eliminates 3–4 redundant DB hits per turn on long sessions.
- **`TurnPhase`** (`src/runner.rs`) — `FirstTurn` / `Subsequent` enum that gates the personality + skills preamble blocks. Replaces a hidden boolean.
- **`Supervisor`** (`src/supervisor.rs`) — orchestrator. On a 10s tick, consults `IdlePolicy` to decide whether to inject work, then `decide_next_steps` picks the next agent. `TerminationPolicy` decides when to stop.
- **`ToolRegistry`** (`src/registry.rs`) — typed dispatch with per-agent allowlists. 18 tools total across 4 categories (see below). `PromptToolTable` (`src/prompt_allowlist.rs`) cross-checks the embedded prompt templates against the allowlists at build time.
- **`MarkerNormalizer`** (`src/marker_normalizer.rs`) — pure function that turns `(raw_op, payload)` into `(canonical_op, payload)`. Handles community alias rewrites and prompt-convention defaults.
- **`Worker`** (`src/worker.rs`) — durable task loop. Claims tasks from `TaskQueue`, dispatches via `ToolRegistry`, panic-safe, lease-protected.
- **`TaskQueue`** (`src/queue.rs`) — SQLite-backed durable queue with leases, retries, idempotency keys.
- **`Memory`** (`src/memory/`) — SQLite-backed handle. Persistence split by table: `events`, `agents`, `semantic_memories`, `behavior_memories`, `sessions`. Plus `context.rs` for the 3-layer retrieval pattern (peek → timeline → observation).

## Tools (18 total)

| Category | File | Tools |
|---|---|---|
| **Memory** | `src/tools/memory.rs` | `save_semantic`, `save_behavior`, `get_context`, `peek_context`, `get_timeline`, `get_observation`, `compress_events` |
| **Research** | `src/tools/research.rs` | `record_hypothesis`, `record_review`, `record_tournament_match` |
| **Curation** | `src/tools/curation.rs` | `archive_observation`, `delete_observation` |
| **Experiment** | `src/tools/experiment.rs` | `design_experiment`, `execute_experiment`, `evaluate_result` |
| **Run-agent** | `src/run_agent.rs` (as a tool) | `run_agent` |
| **Skill loader** | `src/skill_loader.rs` (per-skill) | any `SKILL.md` in the skills dir becomes a `Tool` |

Community aliases (`record_research_plan`, `record_system_feedback`) are rewritten at the `MarkerNormalizer` seam, not registered as separate tools.

## The 7 agents

| Agent | Role | Modes |
|---|---|---|
| `supervisor` | Parses goal, dispatches tasks | `parse_goal` |
| `generation` | Proposes hypotheses via literature + debate | `generation_literature`, `generation_debate` |
| `reflection` | Reviews hypotheses (3 modes) + post-experiment reflection | `reflection_review`, `reflection_observation`, `reflection_verification`, `reflection_on_result` |
| `ranking` | Runs the tournament (pairwise + debate) | `ranking_pairwise`, `ranking_debate` |
| `evolution` | Improves top hypotheses (4 strategies) | `evolution_combine`, `evolution_simplify`, `evolution_feasibility`, `evolution_out_of_box` |
| `metareview` | Synthesizes feedback, writes final overview | `metareview_system`, `metareview_final` |
| `experiment` | Closes the empirical loop | `experiment_design`, `experiment_execute`, `experiment_evaluate` |

## Build and test

```bash
# Build everything
cargo build

# Run the test suite (310 tests total: 228 lib + 63 integration + 19 across other crates)
cargo test --workspace --no-fail-fast

# Just co-scientist (the work)
cargo test -p co-scientist --lib
cargo test -p co-scientist --test integration --features test-helpers

# Lint
cargo clippy -p co-scientist --lib
```

Tests don't need an LLM. Co-scientist requires Node.js ≥ 22 only if you wire the `claude` CLI for live runs.

## Quick start (live)

```bash
# From the repo root
cargo run -p co-scientist -- start --goal "What makes compounds selective for KRAS-G12C over KRAS-G12D?"
```

The supervisor parses the goal into a plan, enqueues initial generation tasks, then the worker pool runs them through the pipeline.

## Repo layout

```
.
├── Cargo.toml              # super-workspace manifest
├── Cargo.lock
├── .cargo/config.toml      # build flags (turso-sqlite math, tokio_unstable)
├── ante-preview/           # AI agent runtime (vendored)
│   └── crates/
│       ├── agent-sdk/      # Claude SDK + transport
│       └── agent-server/   # HTTP/stdio server
├── co-scientist/           # memory layer + 7-agent pipeline + empirical loop
│   ├── Cargo.toml
│   ├── SKILL.md            # LLM-facing skill (system prompt content)
│   ├── CONTEXT.md          # domain glossary (Marker, MarkerNormalizer, …)
│   ├── README.md           # crate-specific readme
│   ├── prompts/            # 18 community-prompt templates (markdown)
│   ├── src/
│   │   ├── runner.rs       # agent loop (TurnPhase, PromptContextCache, run_turn_inner)
│   │   ├── run_agent.rs    # SessionRunners, FOLLOW_UP_SPECS, RunAgentTool
│   │   ├── bus.rs          # EventBus, run_failure_aggregator, FailureStats
│   │   ├── registry.rs     # ToolRegistry + default_allowlist
│   │   ├── supervisor.rs   # orchestrator (idle injection, termination)
│   │   ├── supervisor_bundle.rs # CLI-facing supervisor wiring
│   │   ├── worker.rs       # durable task loop
│   │   ├── queue.rs        # TaskQueue with leases
│   │   ├── marker_normalizer.rs # alias rewrite + scope/summary inference
│   │   ├── prompt_allowlist.rs  # build-time prompt↔allowlist validator
│   │   ├── research_session.rs  # ResearchSessionRepo
│   │   ├── policies.rs     # IdlePolicy, TerminationPolicy (pure structs)
│   │   ├── llm_query.rs    # retry/transient-error classification
│   │   ├── skill.rs        # parse_markers
│   │   ├── skill_loader.rs # discover + load SKILL.md bundles
│   │   ├── hypothesis.rs   # Hypothesis model + repo
│   │   ├── tournament.rs   # pairwise ranking + Elo updates
│   │   ├── promotion.rs    # ConsolidationService
│   │   ├── embeddings.rs   # optional embedding API
│   │   ├── claude_cli.rs   # Claude CLI subprocess wrapper
│   │   ├── elo.rs          # Elo math
│   │   ├── experiment.rs   # ExperimentRepo + RunResult
│   │   ├── db.rs           # schema migrations
│   │   ├── tool.rs         # Tool trait + ToolCtx
│   │   ├── agents.rs       # AGENTS table + per-agent skills field
│   │   ├── prompts.rs      # AgentMode + Prompts registry
│   │   ├── tool.rs         # Tool trait
│   │   ├── main.rs
│   │   ├── lib.rs
│   │   ├── tools/
│   │   │   ├── mod.rs      # builtin_tools() — registers all 18
│   │   │   ├── memory.rs   # 7 memory/retrieval tools
│   │   │   ├── research.rs # 3 structured research tools
│   │   │   ├── curation.rs # 2 destructive curation tools
│   │   │   └── experiment.rs # 3 empirical-loop tools
│   │   └── memory/         # persistence split by table
│   │       ├── mod.rs
│   │       ├── types.rs
│   │       ├── helpers.rs
│   │       ├── events.rs
│   │       ├── agents.rs
│   │       ├── semantic.rs
│   │       ├── behavior.rs
│   │       └── context.rs
│   └── tests/integration.rs
├── ref/                    # upstream reference repos surveyed for design ideas
│   └── (8 reference projects: ai-agent-os, claudelicious, claude-mem, …)
└── reports/                # 7 design reports on memory-system architectures
```

## Recent work

The current branch has three commits beyond `bd522da`:

- **`Deepen runner + bus + run_agent seams; add 48 edge-case tests`** — collapsed turn/turn_stream duplication into `run_turn_inner(TurnStrategy)`, replaced the `shown_startup` boolean with a `TurnPhase` enum, added `PromptContextCache`, wired a real subscriber for `MarkerFailed` (`run_failure_aggregator` + `FailureStats`), added `SessionRunners` per-session Runner cache, extracted the pipeline into `FOLLOW_UP_SPECS`. Plus 48 edge-case tests pinning every failure boundary of the agent loop.
- **`Add experiment agent and 3-stage empirical loop`** — seventh agent (`design → execute → evaluate → reflection_on_result`), `ExperimentRepo` + `RunResult` sandbox runner, supervisor hook to inject experiment_design tasks.
- **`Update README to reflect current state`** — this README.

## Notes

`ante-preview/` is not upstream. The vendored snapshot is for local reproducibility; re-vendor periodically or convert to a submodule.

`co_scientist.db` and `*.db.tmp` are gitignored — they contain local run data and never belong in the repo.

`ref/` and `reports/` are research artifacts, not source code. `ref/` contains upstream projects surveyed for design ideas (memory systems, agent architectures). `reports/` contains the design memos written during the survey.
