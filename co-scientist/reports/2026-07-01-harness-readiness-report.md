# co-scientist/ Harness Readiness Report

**Run timestamp (UTC):** 2026-07-01T09:02:00Z
**Scope:** `co-scientist/` crate (ante-preview/, ref/, reports/ excluded)
**Source spec:** `docs/superpowers/specs/2026-07-01-harness-readiness-co-scientist-design.md`

---

## 1. Executive verdict

**Worst-severity finding:** clean (no findings)
**One-line summary:** co-scientist/ has no findings from the composite audit, but scorecard gaps (memoryUsefulness=34, harnessFit=56, taskCoverage=79) suggest the largest improvement opportunities are in memory hooks and harness layer fit.

---

## 2. 5-dimension scorecard

**Scale:** all dimensions on 0-100 scale (where 100 is best).

| Dimension | Score (0-100) |
|---|---|
| harnessFit | 56 |
| compileConfidence | 90 ⚠ see caveat §7 |
| taskCoverage | 79 |
| toolSafety | 100 |
| memoryUsefulness | 34 |
| estCostPerRunUsd | $0.048 |

---

## 3. 7-section genome

| Section | Value |
|---|---|
| repo_type | rust |
| agent_topology | maintainer, tester, security |
| risk_score (0-1) | 0.37 |
| mcp_surface | local_default_deny |
| test_confidence (0-1) | 0.5 |
| publish_readiness (0-1) | 0.55 |

---

## 4. Top findings

Score-sorted ascending list. Cap at 10.

The composite audit (`metaharness_oia_audit`) returned `data: null` with `exitCode: 0`, indicating a clean baseline — no findings to report from the threat-model + mcp-scan + manifest triad.

**From scorecard gaps (sorted ascending):**

1. **[low]** memoryUsefulness = 34/100 — see Recommendation R1
2. **[low]** harnessFit = 56/100 — see Recommendation R2
3. **[low]** taskCoverage = 79/100 — see Recommendation R3

---

## 5. Recommendations

Sorted by impact (largest gap first):

- **R1 — memoryUsefulness gap (34/100):** The 7-agent pipeline has `Memory` (SQLite), `PromptContextCache`, and `MemoryEvent` flows but lacks explicit memory-hooks evidence in the harness layer. Consider adding `.claude/` memory integration tests and a documented memory-hooks contract.
- **R2 — harnessFit gap (56/100):** The harness fits moderately well but the scorecard suggests friction in at least one of {agent_topology alignment, MCP surface, plugin loadout}. The `agent_topology` from §3 lists `maintainer, tester, security` as roles; the score suggests friction in at least one of these role-specific configurations. Inspect `.claude/agents/` and `co-scientist/agents.rs` for role configs and identify which role's allowlist or skill loadout is incomplete.
- **R3 — taskCoverage (79/100):** Coverage is good but not perfect. Identify the 21% gap (likely the bucket-reshuffled modules per CONTEXT.md, since 91 files are mid-refactor and tests haven't been verified).
- **R4 — empirical verification:** The `compileConfidence = 90/100` score is a structural estimate; the working tree is mid-refactor and `cargo build`/`cargo test` have not been verified against this state. Run those commands before treating the score as empirical.

---

## 6. Provenance

| Item | Value |
|---|---|
| Run timestamp (UTC) | 2026-07-01T09:02:00Z |
| Composite audit tool | metaharness_oia_audit |
| Composite audit exitCode | 0 |
| Composite audit degraded | false |
| Composite audit data | null (clean baseline — no findings) |
| Score tool | metaharness_score |
| Score exitCode | 0 |
| Score degraded | false |
| Score scale | 0-100 integers (spec example assumed 0-1 floats) |
| Score extra fields | schema, repo, recommendedMode, archetype, template, scaffoldReady, hardConstraints, generatedAt, durationMs |
| Genome tool | metaharness_genome |
| Genome exitCode | 0 |
| Genome degraded | false |
| Genome scale | 0-1 floats for risk_score, test_confidence, publish_readiness |
| Genome extra fields | path, durationMs, generatedAt |
| Pre-flight MCP status | running=true, pid=8901, transport="stdio" |
| Pre-flight shape drift | mcp_status returned `{running, pid, transport, port, host}` — no top-level `success` flag, no `data.tools_loaded`. Provenance captures pid + transport instead. |
| Memory namespace | metaharness-audit |
| Memory keys persisted | 2026-07-01T09:02:00Z-oia_audit, 2026-07-01T09:02:00Z-score, 2026-07-01T09:02:00Z-genome |
| Memory timestamp (shared) | 2026-07-01T09:02:00Z |
| **Memory tags parameter** | **Backend bug:** MCP `memory_store` accepts tags but the `sql.js + HNSW` backend silently drops the tags field — verified across two independent insertion attempts (standard insert, and delete-then-fresh-insert). The stored values themselves are correct and retrievable. Tags were not load-bearing for downstream drift detection (which uses fingerprint + value comparison) or for this report composition (which reads by namespace + key). Backend bug filed separately; does not affect this task. |

---

## 7. Caveats

- **`compileConfidence` is unverified empirically**: the working tree is mid-refactor (91 files changed per `co-scientist/../CONTEXT.md`) and `cargo build` / `cargo test` have not been run against this state. Treat `compileConfidence = 90/100` as a structural estimate, not an empirical measurement.
- **Memory backend tag-drop**: as noted in Provenance §6, the ruflo `sql.js + HNSW` memory backend has a confirmed bug where the `tags` parameter is silently dropped. Values persist correctly; tags do not. This affects all ruflo memory users, not just this run.
- **Spec-vs-tool shape drift**: the spec's example response shapes (`{success, data: {...}}` with populated data objects) differ from the actual tool returns (e.g., `data: null` on clean audit runs, score fields on 0-100 scale rather than 0-1 floats). The implementer adapted to actual tool behavior; the report reflects real tool outputs.

---

## 8. Out of scope (not measured)

- `cargo build` / `cargo test` runs (would empirically verify `compileConfidence`)
- `ante-preview/` (vendored, read-only)
- `ref/`, `reports/`
- The TUI crate (excised per `CONTEXT.md`)
- Darwin Mode (`metaharness_evolve`)
- Adversarial red/blue testing (`metaharness_redblue`)
