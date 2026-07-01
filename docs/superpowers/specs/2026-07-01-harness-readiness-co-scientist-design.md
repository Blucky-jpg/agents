# Harness Readiness Analysis — co-scientist/

**Date:** 2026-07-01
**Status:** Design (pre-implementation)
**Author:** Brainstorming session, design approved

## Goal

Produce a baseline readiness & reliability report for the `co-scientist/` crate using ruflo's metaharness tools. The report must support future drift detection (persisted records) and give a clear worst-severity verdict + per-dimension detail. **No code changes.** **No `cargo build` / `cargo test` runs** — those are "fix" territory, not "analyze".

## Scope

- **In scope:** `co-scientist/` crate, its `.claude/` harness layer references
- **Out of scope:** `ante-preview/` (vendored, read-only), `ref/`, `reports/`, the TUI crate (already being excised per CONTEXT.md)

## Approach

Composite-first drill-down:

1. `metaharness_oia_audit` — composite worst-severity verdict
2. `metaharness_score` — 5-dimension numeric scorecard
3. `metaharness_genome` — 7-section categorical readiness report

Each call persists a timestamped record to the `metaharness-audit` memory namespace. All three compose into a single markdown report.

## Architecture

```
co-scientist/ ──┬─► metaharness_oia_audit ─┐
                │                           │
                ├─► metaharness_score ──────┼─► metaharness-audit memory ns
                │                           │   (timestamped JSON per tool)
                └─► metaharness_genome ─────┘
                                            │
                                            ▼
                  co-scientist/METAHARNESS_REPORT.md (committed, dated)
```

Linear flow, each call independent and idempotent.

## Components

### 1. Tool layer (ruflo MCP)

| Tool | Purpose | Output shape |
|---|---|---|
| `metaharness_oia_audit(path="co-scientist")` | Composite of manifest + threat-model + mcp-scan; timestamped record | Worst-severity verdict + per-finding breakdown |
| `metaharness_score(path="co-scientist")` | 5-dim numeric scorecard | `{harnessFit, compileConfidence, taskCoverage, toolSafety, memoryUsefulness, estCostPerRunUsd}` |
| `metaharness_genome(path="co-scientist")` | 7-section categorical report | `{repo_type, agent_topology, risk_score, mcp_surface, test_confidence, publish_readiness}` |

All return the standard `{success, data, degraded, exitCode}` envelope. `degraded:true` means an optional dep (e.g. `@metaharness/*`) is missing — that must be visible in the report, not silently absorbed.

### 2. Persistence layer

- Namespace: `metaharness-audit`
- Key shape: `<ISO8601-timestamp>-<tool-name>` (e.g. `2026-07-01T12:34:56Z-oia_audit`)
- Tags: `["harness-readiness", "co-scientist", "tool:<name>"]`
- Used for future `metaharness_drift_from_history` calls

### 3. Report layer

- Path: `co-scientist/METAHARNESS_REPORT.md` (primary, always written)
- Backup copy: `co-scientist/reports/YYYY-MM-DD-harness-readiness-report.md` for archival; append `-N` suffix on same-day collisions
- Sections, in order:
  1. **Executive verdict** — worst-severity finding + 1-line summary
  2. **5-dimension scorecard** — from `metaharness_score`
  3. **7-section genome** — from `metaharness_genome`
  4. **Top findings** — severity-sorted list with category
  5. **Recommendations** — what to do next (informational, not actions taken)
  6. **Provenance** — tool versions, run timestamps, `degraded` flags, exit codes
  7. **Caveats** (if any) — e.g. mid-refactor state, optional-dep degradation

## Data flow

1. **Pre-flight:** call `mcp__ruflo__mcp_status`. If unhealthy, abort with a clear error.
2. **Composite first:** call `metaharness_oia_audit(path="co-scientist")`. This is the longest single call (~5-15s).
3. **Parallel detail:** call `metaharness_score` and `metaharness_genome` in the same message (parallel, independent).
4. **Persist:** call `mcp__ruflo__memory_store` three times with the timestamp-prefixed keys.
5. **Compose:** transform the three JSON outputs into the markdown report. Pure transformation.
6. **Commit:** stage + commit `co-scientist/METAHARNESS_REPORT.md`.

## Error handling

| Exit code / signal | Meaning | Response |
|---|---|---|
| `exitCode === 0` | success | proceed |
| `exitCode === 1` | alert triggered | record + include in report, continue (alert is the *result*) |
| `exitCode === 2` | input error | abort, surface verbatim, no partial report |
| `degraded:true` | optional dep missing | record under Provenance, do not abort |
| MCP unhealthy | ruflo MCP server not reachable | abort with explicit message |
| Memory store failure | persistence step failed | report still writes; Provenance notes the failure |
| Same-day file collision | report file already exists | append `-N` suffix |

**What we do NOT do on error:**
- Silent retries of failed tool calls (would mask the actual failure)
- Run `cargo build` or `cargo test` to "fix" the compileConfidence score
- Modify source files — only `METAHARNESS_REPORT.md` touches disk

## Verification before completion

Before claiming the run is complete, verify:

1. `mcp__ruflo__mcp_status` returned healthy pre-flight
2. Every tool call's `exitCode` inspected; non-{0,1} codes flagged in Provenance
3. Every `degraded:true` flag recorded under Provenance
4. `compileConfidence` dimension gets a caveat note about the mid-refactor state (91 files changed per CONTEXT.md, `cargo build` unverified)
5. Three distinct, timestamp-prefixed keys exist in `metaharness-audit` namespace
6. `git status` shows `co-scientist/METAHARNESS_REPORT.md` as a new committed file

If any verification step fails, the report gets a "⚠ Caveats" section at the top so the verdict is read with the right context.

## Out of scope (explicitly)

- Running `cargo build` / `cargo test` to verify the bucket reshuffle
- Fixing any findings (e.g. tightening tool allowlists, adding tests)
- Running `metaharness_evolve` (Darwin Mode) — would mutate harness policy
- Running `metaharness_redblue` adversarial testing
- Analyzing `ante-preview/`, `ref/`, `reports/`
- Re-running ruflo MCP integration tests

## Open questions

None — design is complete and approved.