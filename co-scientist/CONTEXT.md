# Co-Scientist Domain Glossary

Domain terms used across this crate's modules. Each term names a concept the
code treats as a first-class idea. The architecture review and the
`/improve-codebase-architecture` command refer to these by name; do not
drift into implementation vocabulary (`fn`, `struct`, `handler`, `service`).

## Marker

The unit a model emits to record a tool call. Format:
`[[MEMORY_OP:<op>:{...json payload...}]]`. Parsed by
[`crate::skill::parse_markers`]. The `op` field is the raw tool name —
which may be a community prompt alias (e.g. `record_research_plan`) or a
first-class tool name (e.g. `save_semantic`).

## MarkerNormalizer

The seam between marker parsing and tool dispatch. Pure function that
turns a `(raw_op, payload)` pair into `(canonical_op, normalized_payload)`,
applying community alias rewrites and prompt-convention defaults (e.g.
`record_research_plan` implies `scope="plan"`). Lives at
`src/marker_normalizer.rs`. Unit-tested without a `Memory` fixture.

Design contract: pure, no DB, no async. Adding a new community alias =
one row in the `ToolAlias` table. Validation that should propagate up as
`MemoryEvent::MarkerFailed` (e.g. missing `summary` on `save_semantic`)
lives here, not in the runner.

## MemoryEvent

A typed variant on the in-process [`EventBus`]. Variants: `MarkerFailed`
(meta-signal that a tool dispatch failed — used by reflection passes to
mine which tools the LLM misuses), `MemoryOpSucceeded`, and others.
Defined in [`crate::bus`].

## Tool (and ToolRegistry)

A typed dispatch surface for memory operations. Each `Tool` declares its
name, JSON-schema input, and an async `call` method. `ToolRegistry`
maintains a typed map plus per-agent allowlists. The marker pipeline feeds
into the registry; the registry is also callable directly (used by
[`RunAgentTool`] and tests).

## Run

A single research session. Identified by `run_id`. Owns a turn trace
(`events` table), all `semantic_memories` and `behavior_memories`
written during it, and the per-run `TaskQueue`. A `ResearchSession`
groups one or more runs against a single goal.

## Tournament

A pairwise ranking mechanism over `Hypothesis` rows. Implemented as a
task-queue job (`record_tournament_match` tool) that scores two
hypotheses and writes an ELO update. See [`crate::tournament`].

## ConsolidationService

Background daemon that promotes hot memories and archives cold ones.
Six phases: embed backfill → cluster → archive → reindex → decay →
upgrade. See [`crate::promotion`]. Identified in IMPROVEMENT_PLAN §3 as
"the load-bearing piece."

## Recent marker errors

The self-correction loop. `Runner` tracks the last N marker dispatch
failures in a per-run scratchpad and surfaces them in the next turn's
system prompt so the LLM can correct course. Wired through `MarkerFailed`
events at the seam.