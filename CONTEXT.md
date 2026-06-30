# CONTEXT — Agents monorepo

Resumption aid after `/clear`. Honest snapshot of where the repo is right now, not where the README claims it is.

## What this is

A local, offline-capable research lab built on two Rust crates:

- **`co-scientist/`** — the work. A 7-agent research pipeline (supervisor, generation, reflection, ranking, evolution, metareview, experiment) over a SQLite-backed memory layer. The agent loop lives here, not in ante.
- **`ante-preview/`** — vendored AI agent runtime snapshot. Not a submodule, not tracking upstream.

Plus research artifacts under `ref/` (upstream projects surveyed) and `reports/` (design memos from that survey). Both are read-only inputs, not source.

## State at a glance

The working tree is **mid-refactor and uncommitted**. 91 file changes against the last commit (`a339078`).

- `co-scientist/src/` got reshuffled into topic-bucket modules
- `co-scientist-tui/` is being **removed**, not renamed — every file in it is staged as deleted
- root `Cargo.toml` no longer lists the TUI as a workspace member
- `co-scientist/IMPROVEMENT_PLAN.md` and `co-scientist/RUN_UNTIL_PAPER_PLAN.md` are **deleted** from disk
- I have not run `cargo build`, `cargo test`, or `cargo clippy` against this tree. The README's "310 tests pass" claim refers to the pre-refactor state; the bucket reshuffle + shim layer may have broken something. **Verify before trusting any test count.**

## The co-scientist/ refactor

Old flat layout (what's committed) → new bucket layout (what's in the working tree):

| Bucket | Contents | Was (still works via shim) |
|---|---|---|
| `agent_loop/` | `runner.rs`, `run_agent.rs`, `mod.rs` | `src/runner.rs`, `src/run_agent.rs` |
| `lifecycle/` | `bus.rs`, `supervisor.rs`, `supervisor_bundle.rs`, `worker.rs`, `queue.rs`, `promotion.rs`, `policies.rs`, `mod.rs` | `src/bus.rs`, `src/supervisor.rs`, `src/worker.rs`, `src/queue.rs`, `src/promotion.rs`, `src/policies.rs` |
| `llm_io/` | `claude_cli.rs`, `embeddings.rs`, `llm_query.rs`, `prompts.rs`, `skill_loader.rs`, `mod.rs` | `src/claude_cli.rs`, `src/embeddings.rs`, `src/llm_query.rs`, `src/prompts.rs`, `src/skill_loader.rs` |
| `marker/` | `normalizer.rs`, `skill.rs`, `allowlist.rs`, `mod.rs` | `src/marker_normalizer.rs`, `src/skill.rs`, `src/prompt_allowlist.rs` |
| `memory/` | (already was a dir) now also holds `db.rs`, `research_session.rs` | `src/db.rs`, `src/research_session.rs` |
| `research/` | `experiment.rs`, `mod.rs` | `src/experiment.rs` |
| `tournament/` | `elo.rs`, `hypothesis.rs`, `matches.rs`, `mod.rs` | `src/elo.rs`, `src/hypothesis.rs` (matches was new) |
| `tools/` | unchanged | unchanged |
| top-level | `agents.rs`, `tool_catalog.rs` | `src/agents.rs`; `tool_catalog.rs` is new |

`src/lib.rs` re-exports from the buckets and defines empty `pub mod NAME { pub use crate::bucket::NAME::*; }` shims for every old flat name. So both `crate::runner::Runner` and `crate::agent_loop::runner::Runner` resolve to the same item. New code should prefer bucket paths; shims exist to keep the diff small.

## The co-scientist-tui/ excision

The TUI was the third workspace member and had its own module subdirs (`render/`, `task/`) staged for renaming. The current decision was to **drop it entirely** rather than land the rename:

- root `Cargo.toml` diff removes `co-scientist-tui` from both `members` and `default-members`
- every file inside `co-scientist-tui/` is listed as `D` or `AD` in `git status`
- the directory still physically exists on disk but the workspace will no longer build it
- if a TUI is wanted later it would be re-added from scratch or from another branch

## README is stale on paths

The root `README.md` (currently `M`) was rewritten in commit `a339078` to describe the pre-refactor layout. It still says things like `src/runner.rs (~2000 LOC)` and names `src/bus.rs`, `src/marker_normalizer.rs`, etc. After the working tree is committed those paths move. The README's *content* (what the pipeline does, what the tools are, how to build) is still accurate; only the file paths are off. Worth fixing in the same commit that lands the reshuffle.

## Planning docs deleted

`co-scientist/IMPROVEMENT_PLAN.md` and `co-scientist/RUN_UNTIL_PAPER_PLAN.md` are gone from the working tree (and from disk). They were top-level strategy docs for this codebase. If the work they described landed, it's now reflected in code and the recent commits; if it didn't, the docs have been discarded. Two newer planning files exist at `.mimocode/plans/` (gitignored, not part of the repo):

- `1782565685429-curious-forest.md` — TUI fixture + ToolCall rendering + marker-leak fix (moot now that the TUI is being removed)
- `1782652361987-misty-falcon.md` — adding the experiment agent and closing the empirical loop

## Recent commits (oldest first, only top-of-interest)

```
c46d7ab Initial commit: co-scientist + ante-preview monorepo
47d4005 Add root README + co-scientist README
22828c6 Add co-scientist-tui crate; extract supervisor wiring into shared bundle
84a4cb0 Extract markdown renderer; split AgentToUi handling into per-variant reducers
24774af Extract marker scrubber to its own module
b0c1843 Push the chat-metrics seam above the lock
38f5b69 Migrate CLI cmd_start to use the supervisor bundle
27fbc14 Extract the TUI agent task into its own module
bd522da Add frame-profile instrumentation for TUI lag diagnostics
c39abb9 Deepen runner + bus + run_agent seams; add 48 edge-case tests
03d4aa8 Add experiment agent and 3-stage empirical loop
35df3fc Update README to reflect current state
a339078 README: comprehensive rewrite reflecting current project  ← HEAD
```

The two commits after `c46d7ab` (`35df3fc`, `a339078`) are README-only.

## Files that matter most

- `co-scientist/src/lib.rs` — bucket map + flat-name shims
- `co-scientist/src/agent_loop/runner.rs` — agent loop (was `src/runner.rs`)
- `co-scientist/src/agent_loop/run_agent.rs` — `FOLLOW_UP_SPECS`, `SessionRunners`, `RunAgentTool`
- `co-scientist/src/lifecycle/supervisor.rs` — orchestrator
- `co-scientist/src/lifecycle/worker.rs` — durable task loop
- `co-scientist/src/lifecycle/queue.rs` — `TaskQueue`
- `co-scientist/src/marker/normalizer.rs` — alias rewrite seam
- `co-scientist/CONTEXT.md` — domain glossary (Marker, MarkerNormalizer, MemoryEvent, Tool, Run, Tournament, ConsolidationService, recent marker errors). Read this before reading code.

## Caveats

- Don't quote test counts or build status from the README. Verify in this tree.
- `co_scientist.db`, `co_scientist.db-shm`, `co_scientist.db-wal` sit at the repo root. Gitignored, never commit. There's also `co_scientist.db.tmp/` — a temp workspace.
- `ref/` and `reports/` are research inputs, not code. Don't import from them.
- The bucket reshuffle preserved behaviour by `pub use`, not by recompiling — shim correctness has never been tested with `cargo build` from this tree (I haven't run it). First thing to do after resuming: `cargo build` and `cargo test -p co-scientist --lib`.
