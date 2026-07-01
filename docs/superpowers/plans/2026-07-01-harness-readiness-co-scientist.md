# Harness Readiness Analysis — co-scientist/ Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Produce a baseline readiness & reliability report for `co-scientist/` using ruflo's metaharness tools (composite-first drill-down), persist findings to `metaharness-audit` memory namespace, and commit a dated markdown report.

**Architecture:** Linear flow — pre-flight MCP check → composite `oia_audit` → parallel `score` + `genome` → persist 3 records → compose report → commit. No source code is modified; only new report files touch disk.

**Tech Stack:** ruflo MCP tools (`metaharness_*`, `mcp_status`, `memory_store`, `memory_list`), bash, git.

**Worktree note:** This plan was brainstormed without a worktree. The work touches only new files (`co-scientist/METAHARNESS_REPORT.md` + dated backup), so uncommitted changes in the working tree are unaffected. If you want isolation anyway, run this plan inside `git worktree add`.

---

## File Structure

| Path | Status | Purpose |
|---|---|---|
| `co-scientist/METAHARNESS_REPORT.md` | Create (new) | Primary report — always reflects latest run |
| `co-scientist/reports/2026-07-01-harness-readiness-report.md` | Create (new) | Dated archival copy; `-N` suffix on collisions |
| `metaharness-audit` memory namespace | Create (new) | 3 timestamped records for drift detection |

No source files are modified.

---

## Task 1: Pre-flight — verify ruflo MCP is reachable

**Files:** none

- [ ] **Step 1: Call `mcp__ruflo__mcp_status`**

Pass no arguments. Expected response shape:
```json
{
  "success": true,
  "data": { "tools_loaded": <int>, "transport": "<string>", "...": "..." }
}
```

- [ ] **Step 2: Verify `success === true` and `transport` is not null**

If `success` is `false` or `transport` indicates "disconnected":
- Abort the entire plan with the message: "Ruflo MCP server is not reachable — start it with `npx ruflo@latest mcp start` or check the MCP config in `.claude/settings.local.json`."
- Do NOT proceed to Task 2.

- [ ] **Step 3: Record the MCP status response for the Provenance section**

Hold the response JSON aside — its `tool_count` (or equivalent) goes into the report's Provenance block under "ruflo MCP server version/transport".

---

## Task 2: Run composite audit

**Files:** none (output captured in-task)

- [ ] **Step 1: Call `mcp__ruflo__metaharness_oia_audit`**

Arguments:
```json
{
  "path": "co-scientist",
  "dryRun": false
}
```

Expected response shape:
```json
{
  "success": true,
  "data": {
    "worstSeverity": "clean|low|medium|high",
    "findings": [...],
    "fingerprint": "<string>"
  },
  "degraded": false,
  "exitCode": 0
}
```

- [ ] **Step 2: Inspect exitCode**

- `exitCode === 0` → continue, record under Provenance
- `exitCode === 1` → alert was triggered; the findings ARE the result; continue
- `exitCode === 2` → abort with verbatim error from `data` field; do NOT proceed to Task 3

- [ ] **Step 3: Inspect `degraded` flag**

If `degraded === true`, note in Provenance: "Composite audit ran without optional @metaharness/* deps; treat severity as upper bound."

- [ ] **Step 4: Capture the full response JSON for later composition**

Hold the response. It will be combined with Task 3 outputs in Task 5.

---

## Task 3: Run score and genome in parallel

**Files:** none (outputs captured in-task)

- [ ] **Step 1: Call both tools in a single message (parallel)**

Tool call A:
```json
{
  "tool": "mcp__ruflo__metaharness_score",
  "arguments": { "path": "co-scientist" }
}
```

Tool call B (in the same message):
```json
{
  "tool": "mcp__ruflo__metaharness_genome",
  "arguments": { "path": "co-scientist" }
}
```

Both are independent and run faster when fired together. Expected response shapes:

`metaharness_score`:
```json
{
  "success": true,
  "data": {
    "harnessFit": <float 0-1>,
    "compileConfidence": <float 0-1>,
    "taskCoverage": <float 0-1>,
    "toolSafety": <float 0-1>,
    "memoryUsefulness": <float 0-1>,
    "estCostPerRunUsd": <float>
  },
  "degraded": false,
  "exitCode": 0
}
```

`metaharness_genome`:
```json
{
  "success": true,
  "data": {
    "repo_type": "<string>",
    "agent_topology": "<string>",
    "risk_score": <float>,
    "mcp_surface": "<string>",
    "test_confidence": "<string>",
    "publish_readiness": "<string>"
  },
  "degraded": false,
  "exitCode": 0
}
```

- [ ] **Step 2: Verify each tool's exitCode**

Same rules as Task 1 Step 2 — `0` or `1` continue, `2` abort.

- [ ] **Step 3: Verify each tool's `degraded` flag**

Same rules as Task 1 Step 3.

- [ ] **Step 4: Capture both responses for later composition**

Hold both JSONs for Task 5.

---

## Task 4: Persist three records to `metaharness-audit` namespace

**Files:** none (writes to `.swarm/memory.db` via ruflo MCP)

- [ ] **Step 1: Generate a single ISO8601 UTC timestamp**

Use `date -u +"%Y-%m-%dT%H:%M:%SZ"` to generate the timestamp. Example output: `2026-07-01T14:23:45Z`. Use the SAME timestamp for all three records so they cluster.

- [ ] **Step 2: Persist the composite audit**

Tool call:
```json
{
  "tool": "mcp__ruflo__memory_store",
  "arguments": {
    "key": "<TIMESTAMP>-oia_audit",
    "value": "<full Task 2 response as JSON string>",
    "namespace": "metaharness-audit",
    "tags": ["harness-readiness", "co-scientist", "tool:oia_audit"]
  }
}
```

Replace `<TIMESTAMP>` with the value from Step 1. Replace `<full Task 2 response as JSON string>` with the captured response.

- [ ] **Step 3: Persist the score**

Tool call:
```json
{
  "tool": "mcp__ruflo__memory_store",
  "arguments": {
    "key": "<TIMESTAMP>-score",
    "value": "<full Task 3 score response as JSON string>",
    "namespace": "metaharness-audit",
    "tags": ["harness-readiness", "co-scientist", "tool:score"]
  }
}
```

- [ ] **Step 4: Persist the genome**

Tool call:
```json
{
  "tool": "mcp__ruflo__memory_store",
  "arguments": {
    "key": "<TIMESTAMP>-genome",
    "value": "<full Task 3 genome response as JSON string>",
    "namespace": "metaharness-audit",
    "tags": ["harness-readiness", "co-scientist", "tool:genome"]
  }
}
```

- [ ] **Step 5: Verify three records exist**

Tool call:
```json
{
  "tool": "mcp__ruflo__memory_list",
  "arguments": { "namespace": "metaharness-audit", "limit": 10 }
}
```

Expected: the three keys from Steps 2-4 appear in the list, all timestamped identically, each with `tool:<name>` tag.

If any of the three keys is missing, do not abort — note the missing persistence in Provenance and continue to Task 5. The report still gets written; only the memory persistence failed.

---

## Task 5: Compose the markdown report

**Files:**
- Create: `co-scientist/METAHARNESS_REPORT.md`
- Create: `co-scientist/reports/2026-07-01-harness-readiness-report.md`

- [ ] **Step 1: Generate ISO8601 date for the dated backup filename**

Run `date -u +"%Y-%m-%d"`. Example: `2026-07-01`.

- [ ] **Step 2: Check for filename collision**

Run `ls co-scientist/reports/2026-07-01-harness-readiness-report.md 2>/dev/null`. If the file exists, append `-N` where N starts at 2 and increments until the path does not exist.

- [ ] **Step 3: Write `co-scientist/METAHARNESS_REPORT.md`**

Use the template below. Fill every placeholder `<...>` from the captured Task 2-3 outputs. Do NOT leave any `<...>` in the final file.

```markdown
# co-scientist/ Harness Readiness Report

**Run timestamp (UTC):** <TIMESTAMP>
**Scope:** `co-scientist/` crate (ante-preview/, ref/, reports/ excluded)
**Source spec:** `docs/superpowers/specs/2026-07-01-harness-readiness-co-scientist-design.md`

---

## 1. Executive verdict

**Worst-severity finding:** <WORST_SEVERITY>
**One-line summary:** <ONE_LINE_SUMMARY>

---

## 2. 5-dimension scorecard

| Dimension | Score |
|---|---|
| harnessFit | <HF> |
| compileConfidence | <CC> ⚠ see caveat §7 |
| taskCoverage | <TC> |
| toolSafety | <TS> |
| memoryUsefulness | <MU> |
| estCostPerRunUsd | $<COST> |

---

## 3. 7-section genome

| Section | Value |
|---|---|
| repo_type | <RT> |
| agent_topology | <AT> |
| risk_score | <RS> |
| mcp_surface | <MS> |
| test_confidence | <TCD> |
| publish_readiness | <PR> |

---

## 4. Top findings

Severity-sorted list. Cap at 10.

1. **[<SEV>]** <FINDING_TITLE> — <FINDING_DETAIL>
2. **[<SEV>]** <FINDING_TITLE> — <FINDING_DETAIL>
...

---

## 5. Recommendations

- <REC_1>
- <REC_2>
- <REC_3>

(Informational only — no actions taken in this run.)

---

## 6. Provenance

| Item | Value |
|---|---|
| Run timestamp | <TIMESTAMP> |
| Composite audit tool | metaharness_oia_audit |
| Composite audit exitCode | <OA_EC> |
| Composite audit degraded | <OA_DEG> |
| Score tool | metaharness_score |
| Score exitCode | <SC_EC> |
| Score degraded | <SC_DEG> |
| Genome tool | metaharness_genome |
| Genome exitCode | <GN_EC> |
| Genome degraded | <GN_DEG> |
| Memory namespace | metaharness-audit |
| Memory keys persisted | <LIST OF KEYS> |
| ruflo MCP status (pre-flight) | <HEALTHY/UNHEALTHY> |

---

## 7. Caveats

- **`compileConfidence` is unverified**: the working tree is mid-refactor (91 files changed per `co-scientist/../CONTEXT.md`) and `cargo build` has not been run against this state. Treat `compileConfidence` as a structural estimate, not an empirical measurement.
- <OTHER_CAVEATS if any tools reported degraded:true>

---

## 8. Out of scope (not measured)

- `cargo build` / `cargo test` runs (would fix `compileConfidence`)
- `ante-preview/` (vendored, read-only)
- `ref/`, `reports/`
- The TUI crate (excised per `CONTEXT.md`)
- Darwin Mode (`metaharness_evolve`)
- Adversarial red/blue testing (`metaharness_redblue`)
```

- [ ] **Step 4: Verify the report has no unfilled placeholders**

Run `grep -n '<[A-Z_]\+>' co-scientist/METAHARNESS_REPORT.md`. Expected output: nothing.

If any match found, the report has unfilled placeholders. Fix them inline by reading the relevant tool output and substituting.

- [ ] **Step 5: Copy report to dated backup**

Run:
```bash
cp co-scientist/METAHARNESS_REPORT.md co-scientist/reports/<FILENAME_FROM_STEP_2>
```

- [ ] **Step 6: Verify both files exist**

Run:
```bash
ls -la co-scientist/METAHARNESS_REPORT.md co-scientist/reports/<FILENAME_FROM_STEP_2>
```

Expected: both files present with non-zero size.

---

## Task 6: Verification gate

**Files:** none (read-only checks)

- [ ] **Step 1: Confirm pre-flight MCP status was healthy**

Pull the response from Task 1 Step 3. `success === true` and `transport` non-null.

- [ ] **Step 2: Confirm all tool exitCodes were {0, 1}**

Pull the exitCodes from Tasks 2 and 3. Any value outside `{0, 1}` is a red flag — note it under §7 Caveats of the report if not already there.

- [ ] **Step 3: Confirm any `degraded:true` flags are recorded under §6 Provenance**

Cross-check: every `degraded:true` from Tasks 2-3 must appear in §6 of the report. If missing, add it.

- [ ] **Step 4: Confirm §7 Caveats contains the `compileConfidence` unverified note**

Mandatory. If missing, add it.

- [ ] **Step 5: Confirm three distinct, timestamp-prefixed keys exist in `metaharness-audit`**

Re-run `mcp__ruflo__memory_list` and confirm three keys with the same timestamp prefix and three distinct tool-name suffixes. If any missing, note in Provenance and continue.

- [ ] **Step 6: Confirm both report files exist and have content**

```bash
ls -la co-scientist/METAHARNESS_REPORT.md co-scientist/reports/*.md
wc -l co-scientist/METAHARNESS_REPORT.md
```

Expected: both files exist; primary file has 50+ lines.

---

## Task 7: Commit and report inline

**Files:** both report files (already created)

- [ ] **Step 1: Stage both report files**

```bash
cd /home/blucky/Agents
git add co-scientist/METAHARNESS_REPORT.md
git add co-scientist/reports/2026-07-01-harness-readiness-report.md
git status --short
```

Expected: both files show as `A` (added).

- [ ] **Step 2: Commit**

```bash
git -c user.name=agent -c user.email=agent@local commit -m "$(cat <<'EOF'
co-scientist: harness readiness report (baseline)

Generated via metaharness_oia_audit + score + genome against co-scientist/
only. Records persisted to metaharness-audit memory namespace. No source
modified. compileConfidence caveat included — working tree is mid-refactor
and cargo build has not been verified.

Spec: docs/superpowers/specs/2026-07-01-harness-readiness-co-scientist-design.md
EOF
)"
```

Expected: commit succeeds with one new file (or two if both are listed).

- [ ] **Step 3: Verify commit landed**

```bash
git log --oneline -3
```

Expected: the new commit appears at HEAD.

- [ ] **Step 4: Reply inline to the user**

Format:
```
Harness readiness analysis complete for co-scientist/.

Worst-severity finding: <WORST_SEVERITY>
Readiness score (harnessFit): <HF>

Top 3 findings:
1. <F1>
2. <F2>
3. <F3>

Report: co-scientist/METAHARNESS_REPORT.md (commit <SHA>)
Records: 3 in metaharness-audit namespace
Next step: <recommendation>
```