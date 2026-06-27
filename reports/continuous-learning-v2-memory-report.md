# Continuous Learning v2 — Memory & Evolution Report

> Investigation of `ref/continuous-learning-v2/` — an instinct-based learning system that
> observes Claude Code sessions, extracts atomic "instincts" with confidence scoring,
> and evolves them into skills/commands/agents.

**Project location:** `ref/continuous-learning-v2/`
**Version:** 2.1.0 (project-scoped instincts)
**Total LOC:** ~3343 lines across 5 source files
**Live status:** observer disabled in `config.json` (no data has been written yet)

---

## 1. Repository Layout

```
ref/continuous-learning-v2/
├── SKILL.md                 # Skill metadata, when-to-activate, command reference
├── config.json              # Observer config (enabled=false, interval=5, min_obs=20)
├── hooks/
│   └── observe.sh           # PreToolUse/PostToolUse hook — captures every tool call
├── agents/
│   ├── observer.md          # Background Haiku agent instructions
│   ├── observer-loop.sh     # Long-running analysis loop
│   ├── start-observer.sh    # Process launcher (start/stop/status, --reset)
│   └── session-guardian.sh  # Pre-analysis gate (time, cooldown, idle)
└── scripts/
    ├── detect-project.sh    # Project context resolution (bash)
    ├── migrate-homunculus.sh# One-shot v2.0 → v2.1 path migration
    ├── instinct-cli.py      # 1914-line CLI: status/import/export/evolve/promote/projects/prune
    ├── test_parse_instinct.py # 1045-line pytest suite
    └── lib/
        └── homunculus-dir.sh  # Shared XDG dir resolver
```

---

## 2. Memory Architecture

The system implements a **two-tier memory model**:

| Tier | Location | Scope | Source |
|------|----------|-------|--------|
| **Working memory** | `${PROJECT_DIR}/observations.jsonl` | Per-project JSONL stream | Hook captures |
| **Long-term memory** | `instincts/*.yaml` + `evolved/{skills,commands,agents}/` | Atomic, confidence-scored files | Observer-derived |
| **Index** | `${HOMUNCULUS_DIR}/projects.json` + `<project>/project.json` | Hash → metadata registry | Auto-updated |

### 2.1 Data Directory Resolution

**File:** `scripts/lib/homunculus-dir.sh:9-31`

Resolution precedence (avoids Claude Code's sensitive-path guard that blocks `~/.claude/`):

```
1. CLV2_HOMUNCULUS_DIR  (env override, must be absolute)
2. XDG_DATA_HOME/ecc-homunculus
3. ~/.local/share/ecc-homunculus   ← default
```

The legacy v2.0 path `~/.claude/homunculus/` is still readable but new writes go to the
XDG path. A one-shot migration is provided: `scripts/migrate-homunculus.sh:1-62` —
refuses to run if `observer-loop.sh` is alive (uses `pgrep`).

### 2.2 Project Detection

**File:** `scripts/detect-project.sh:93-235` + `scripts/instinct-cli.py:284-394`

Each project gets a **12-char SHA-256 prefix** as ID. Resolution priority:

1. `CLAUDE_PROJECT_DIR` env var → git root
2. `git rev-parse --show-toplevel` from CWD
3. Fallback → `id="global"`

Hash input priority (for cross-machine stability):
1. **Normalized git remote URL** (lowercase, credentials stripped, `.git` suffix removed) — portable
2. Main worktree root path (for worktree-linked repos without remote) — machine-specific
3. SHA-256 computed in Python (`hashlib.sha256(...).hexdigest()[:12]`) to avoid shell locale issues
   (see `detect-project.sh:171-181` and `instinct-cli.py:110-111`)

The detection function performs a **legacy hash migration** (`detect-project.sh:192-217`):
if the new normalized hash has no directory, it checks legacy inputs (raw remote URL,
credential-stripped remote) and `mv`s the old directory to the new ID. This handles
users upgrading from earlier homunculus versions.

### 2.3 Storage Layout

```
~/.local/share/ecc-homunculus/
├── projects.json                 # Global registry: {id: {name, root, remote, last_seen, created_at}}
├── identity.json                 # User profile (referenced in SKILL.md; not in source)
├── observations.jsonl            # Global fallback observations (when CLV2_NO_PROJECT=1)
├── instincts/
│   ├── personal/                 # Auto-learned (global scope)
│   └── inherited/                # Imported from external sources (global scope)
├── evolved/
│   ├── skills/
│   ├── commands/
│   └── agents/
└── projects/<12-char-hash>/
    ├── project.json              # Per-project metadata mirror (atomic writes)
    ├── observations.jsonl        # All tool calls in this project
    ├── observations.archive/     # Rotated at 10MB or after analysis
    ├── instincts/
    │   ├── personal/             # Project-scoped auto-learned
    │   ├── inherited/            # Project-scoped imported
    │   └── pending/              # Awaiting review (TTL-pruned)
    └── evolved/
        ├── skills/
        ├── commands/
        └── agents/
```

---

## 3. Capture Pipeline (How Memory Gets Written)

```
                    ┌─────────────────────────────────────────┐
                    │  Claude Code session (git repo)         │
                    └─────────────────────────────────────────┘
                                       │
            PreToolUse ───► hooks/observe.sh ───► PostToolUse
                                       │
                          (each invocation: ~30ms)
                                       │
                                       ▼
            ┌─────────────────────────────────────────┐
            │  stdin JSON → Python parses → scrubs    │
            │  secrets (api_key/token/...) → appends  │
            │  to observations.jsonl                  │
            └─────────────────────────────────────────┘
                                       │
                                       │ throttled SIGUSR1 every N=20 calls
                                       ▼
            ┌─────────────────────────────────────────┐
            │  observers/observer-loop.sh (Haiku)     │
            │  reads tail-500 of observations.jsonl   │
            │  writes YAML instincts via Write tool   │
            └─────────────────────────────────────────┘
                                       │
                                       │ archive processed file
                                       ▼
            observations.archive/processed-<ts>-<pid>.jsonl
```

### 3.1 The Hook — `hooks/observe.sh` (522 lines)

The hook fires on **every** `PreToolUse` and `PostToolUse` event. Multiple session-guard
layers prevent automation pollution:

| Layer | Gate | Location |
|-------|------|----------|
| 1 | Entrypoint filter (cli/sdk-ts/claude-desktop/claude-vscode only) | `observe.sh:157-160` |
| 2 | `ECC_HOOK_PROFILE=minimal` skip | `observe.sh:163` |
| 3 | `ECC_SKIP_OBSERVE=1` cooperative skip | `observe.sh:166` |
| 4 | Subagent sessions (`agent_id` present) | `observe.sh:169-170` |
| 5 | Path exclusions (`observer-sessions,.claude-mem`) | `observe.sh:172-182` |
| 6 | `disabled` sentinel file | `observe.sh:141-146` |

**Python resolution** is stub-aware (Windows AppInstaller detection,
`observe.sh:42-67`) — the system silently skips Windows Store Python stub that
hangs on `-c`. The chosen interpreter is exported via `CLV2_PYTHON_CMD` to prevent
`detect-project.sh` from re-resolving.

**Secret scrubbing** uses a linear-time regex (commented "fix #2278 — catastrophic
backtracking pegged Python at 100% CPU"). The matcher is bounded: only fixed auth
schemes (bearer/basic/token/bot), quantifier capped at 256 chars
(`observe.sh:270-279`, `observe.sh:340-347`).

**Self-termination** with `signal.alarm(8)` — 2 seconds before Claude Code's 10s
async hook timeout can orphan the subprocess (`observe.sh:282-288`,
`observe.sh:319-325`).

**Observation file rotation** at 10 MB via atomic rename
(`observe.sh:298-305`); 30-day auto-purge of `observations-*.jsonl`
(`observe.sh:204-208`).

**Observer lazy-start** uses flock (Linux), lockfile (BSD), or mkdir-based
lock (POSIX fallback) to prevent double-spawn races
(`observe.sh:433-472`).

**Throttled signaling** — `ECC_OBSERVER_SIGNAL_EVERY_N=20` defaults prevent
rapid-fire SIGUSR1 from spawning runaway parallel Claude processes
(`observe.sh:478-495`).

### 3.2 The Observer Loop — `agents/observer-loop.sh` (335 lines)

Long-running background loop with a triple-guard against runaway spawning
(commented "Fix for #521"):

1. **Re-entrancy guard** — if already analyzing, SIGUSR1 sets `PENDING_ANALYSIS=1`
   and the loop schedules a follow-up analysis after the current one finishes
   (`observer-loop.sh:272-285`).
2. **Cooldown** — `ANALYSIS_COOLDOWN=60s` between analyses
   (`observer-loop.sh:17`, `observer-loop.sh:286-292`).
3. **Tail-based sampling** — only the last `MAX_ANALYSIS_LINES=500` observations
   are passed to the LLM, preventing multi-MB payloads (`observer-loop.sh:139-145`).

**Per-cycle flow:**
1. Check session leases (`SESSION_LEASE_DIR`) — exit if idle without active
   sessions for `IDLE_TIMEOUT_SECONDS=1800` (`observer-loop.sh:51-85`).
2. `wc -l observations.jsonl` — skip if below `MIN_OBSERVATIONS=20`.
3. Run `session-guardian.sh` (3-gate: time window / cooldown / idle detection).
4. Sample last 500 lines into a tmp file, spawn `claude --model haiku --print` with
   the analysis prompt (`observer-loop.sh:153-193`).
5. Auto-scaled `max_turns = clamp(analysis_count/10, 20, 100)` — old hardcoded 20
   was insufficient for 500-line default (`observer-loop.sh:212-218`).
6. Watchdog kills claude after `ECC_OBSERVER_TIMEOUT_SECONDS=120`.
7. Archives `observations.jsonl` → `processed-<ts>-<pid>.jsonl`.

**Pruning** — runs `instinct-cli.py prune --quiet` on observer startup
(`observer-loop.sh:304-306`).

### 3.3 The Observer Prompt — `agents/observer-loop.sh:153-193`

The prompt instructs Haiku to:
- Read the analysis file (relative path for cross-platform MSYS compatibility,
  `observer-loop.sh:147-150`)
- Identify patterns: user corrections, error resolutions, repeated workflows, tool prefs
- For each pattern with 3+ occurrences, **directly write** a YAML instinct via Write tool
- Use the mandated frontmatter format with confidence calibrated to frequency
  (3-5=0.5, 6-10=0.7, 11+=0.85)
- Decide scope per the domain table (security/workflow/git → global; language
  conventions → project)
- Merge with existing instincts rather than duplicating

### 3.4 The Instinct Format

YAML-frontmatter + markdown body, parsed by `parse_instinct_file()`
(`instinct-cli.py:471-522`):

```yaml
---
id: prefer-functional-style
trigger: "when writing new functions"
confidence: 0.7
domain: "code-style"
source: "session-observation"
scope: project
project_id: "a1b2c3d4e5f6"
project_name: "my-react-app"
---
```

The parser treats `---` strictly as frontmatter boundary (warns in comment:
"instinct body must use `***` or `___` for horizontal rules" —
`instinct-cli.py:475-477`).

---

## 4. The Confidence System (How Memory Is Weighted)

**File:** `SKILL.md:307-326`, `agents/observer.md:137-149`

| Score | Meaning | Behavior |
|-------|---------|----------|
| 0.3 | Tentative | Suggested but not enforced |
| 0.5 | Moderate | Applied when relevant |
| 0.7 | Strong | Auto-approved for application |
| 0.9 | Near-certain | Core behavior |

**Initial confidence is calibrated to frequency** (set by observer prompt,
`observer-loop.sh:163-165`):
- 3-5 observations → 0.5
- 6-10 observations → 0.7
- 11+ observations → 0.85

**Confidence evolves over time** (specified in observer.md, but actual mutation
code lives outside this skill — the observer prompt instructs Haiku to update
the YAML directly):
- +0.05 per confirming observation
- -0.1 per contradicting observation
- -0.02 per week without observation (decay)

**Status display** uses Unicode block characters with a CLI-stream-encoding
fallback for non-UTF-8 terminals (`_confidence_bar`, `instinct-cli.py:99-107`).

---

## 5. Evolution Pipeline (How Memory Becomes Skill)

This is the **core improvement mechanism**. Three stages:

### 5.1 Stage 1 — Pattern Detection (Observer)

The observer agent analyzes observations and writes instincts (covered above).
This is **continuous**, not batched.

### 5.2 Stage 2 — Cross-Project Pattern Mining (`/evolve`)

**File:** `scripts/instinct-cli.py:1096-1205` (`cmd_evolve`)

When invoked, it:

1. Loads all instincts (project + global) via `load_all_instincts()`
   (`instinct-cli.py:641-673`) — project-scoped wins over global on ID conflict.
2. **Clusters by normalized trigger** — strips `when/creating/writing/adding/
   implementing/testing` and groups (`instinct-cli.py:1127-1133`).
3. Identifies **skill candidates** — clusters with 2+ instincts, sorted by size
   then avg confidence.
4. Identifies **command candidates** — workflow-domain instincts with
   confidence ≥ 0.7.
5. Identifies **agent candidates** — clusters with 3+ instincts and avg
   confidence ≥ 0.75.
6. Calls `_show_promotion_candidates()` for cross-project promotion candidates.
7. With `--generate`, materializes the structures to disk (see Stage 4).

### 5.3 Stage 3 — Cross-Project Promotion (`/promote`)

**File:** `scripts/instinct-cli.py:1275-1413`

An instinct graduates from `scope: project` to `scope: global` when:

```
PROMOTE_MIN_PROJECTS = 2       # instinct-cli.py:127
PROMOTE_CONFIDENCE_THRESHOLD = 0.8   # instinct-cli.py:126
```

Two modes:
- **Specific** (`promote <instinct-id>`) — single instinct from current project.
- **Auto** (no args) — scans `_find_cross_project_instincts()`
  (`instinct-cli.py:1212-1236`), finds IDs appearing in 2+ projects with avg
  confidence ≥ 0.8, picks highest-confidence version, writes to
  `GLOBAL_PERSONAL_DIR/<id>.yaml` with `promoted_from` and `seen_in_projects`
  provenance fields.

### 5.4 Stage 4 — Materialization (`_generate_evolved`)

**File:** `scripts/instinct-cli.py:1614-1685`

When `evolve --generate` is invoked, three artifact types are written:

| Artifact | Path | Content |
|----------|------|---------|
| **Skill** | `evolved/skills/<name>/SKILL.md` | Title + trigger + bullet list of instinct actions |
| **Command** | `evolved/commands/<name>.md` | Title + provenance + raw instinct content |
| **Agent** | `evolved/agents/<name>.md` | YAML frontmatter (`model: sonnet`, `tools: Read,Grep,Glob`) + source instinct list |

All artifacts carry provenance: "Evolved from N instincts (avg confidence: X%)"
— making the lineage of any generated artifact traceable back to its source
observations.

### 5.5 Pending TTL & Pruning

**File:** `scripts/instinct-cli.py:1692-1827`

Pending instincts (those staged for review) get a 30-day TTL (`PENDING_TTL_DAYS`).
The `prune` command is run on observer startup. Status display warns when pending
instincts exceed 5 or are within 7 days of expiry.

---

## 6. Import / Export (Cross-System Memory Sharing)

**Files:** `instinct-cli.py:834-1016` (import), `instinct-cli.py:1023-1089` (export)

**Security-conscious import:**
- `_validate_file_path()` blocks system dirs (`/etc`, `/usr`, `/var/log`, ...)
  (`instinct-cli.py:147-171`).
- `_validate_instinct_id()` prevents path traversal in filenames
  (`instinct-cli.py:174-184`).
- `_validate_import_url()` requires HTTPS, resolves hostname, and blocks
  private/loopback/link-local/multicast IPs (`instinct-cli.py:187-216`).
- `_fetch_import_url()` caps response at 2 MB, validates content-type, 15s timeout
  (`instinct-cli.py:219-233`).

**Import deduplication** (`instinct-cli.py:885-908`):
- Within source: keep highest confidence per ID
- Against existing: skip if same/higher confidence already present, update if import
  has higher confidence
- Stale files deleted only after the new file is written successfully
  (write-then-delete pattern, `instinct-cli.py:1003-1008`)

**YAML serialization** uses double-quoted strings with embedded-quote escaping
(`_yaml_quote`, `instinct-cli.py:236-243`).

---

## 7. Project Lifecycle Operations

**File:** `scripts/instinct-cli.py:1416-1607`

| Command | Purpose | Lines |
|---------|---------|-------|
| `projects` | List all known projects with counts | 1420-1470 |
| `projects delete <id>` | Remove a project + storage (with `--dry-run`, `--force`) | 1473-1505 |
| `projects gc` | Bulk-delete zero-value projects (no instincts/observations) | 1508-1540 |
| `projects merge <from> <into>` | Move instincts + append observations + drop source | 1543-1607 |

Merge logic (`_merge_instinct_dir`, `instinct-cli.py:590-616`):
- Skip files whose IDs already exist in destination
- On filename collision, append `<sha256-hash-prefix>` to disambiguate
- Observations are **appended** (not replaced) — preserves history

Registry writes use **fcntl flock** for atomic read-modify-write
(`instinct-cli.py:407-435`).

---

## 8. Key Design Decisions (Why It Works This Way)

1. **Hooks vs Skills for observation** — v1 used skills (probabilistic, 50-80%
   reliability). v2 hooks fire 100% deterministically on every tool call
   (`SKILL.md:328-335`). The trade-off is hook overhead, mitigated by throttling.

2. **Project-scoped instincts** — v2.1 prevents cross-project contamination
   (React patterns stay in your React project). Universal patterns go to global
   scope. Promotion happens only after 2+ projects independently develop the
   same instinct (`instinct-cli.py:127`, `agents/observer.md:150-157`).

3. **Atomic confidence-scored instincts instead of full skills** — v1 wrote
   complete skills directly. v2 writes small YAML files with confidence, then
   **clusters them later**. This makes evolution reversible (you can drop a low-
   confidence instinct) and traceable (each skill traces to N instincts).

4. **Background agent (Haiku) for analysis** — keeps the main context window
   free. Cost-optimized with tail-500 sampling and 60s cooldown
   (`observer-loop.sh:17`, `observer-loop.sh:139-145`).

5. **XDG data directory** — moves writes outside `~/.claude/` to avoid Claude
   Code's sensitive-path guard from blocking the background observer
   (`scripts/lib/homunculus-dir.sh:1-31`).

6. **Stale PID recovery** — observer checks `kill -0 <pid>` before trusting PID
   file; on dead process, removes the stale file (`observe.sh:379-398`).

7. **Self-observation avoidance** — observer's own Haiku session sets
   `ECC_SKIP_OBSERVE=1 ECC_HOOK_PROFILE=minimal` so its own tool calls don't
   pollute observations (`observer-loop.sh:242`).

8. **Confirmation-prompt detection** — launcher greps observer log for prompt-
   seeking patterns after start and aborts the observer if found, writing a
   sentinel for human review (`start-observer.sh:55-57`, `:222-227`).

---

## 9. Observed Limitations & Failure Modes

These are **honest** assessments, not sugar-coating:

1. **No semantic search over instincts.** The only retrieval is
   `cmd_status` + human eyeballing. `co-scientist/Co-Scientist` has BM25/TF-IDF
   over observations; `claude-mem` has vector embeddings. This system has
   neither — instincts are essentially a tagged file collection
   (`instinct-cli.py:692-762`). This is the **biggest gap**.

2. **Confidence decay is documented but not implemented in code.** The decay
   formula is specified in `agents/observer.md:143-148`, but there is no
   scheduled job to actually run the decay. The observer only **adds** confidence
   (+0.05 per confirming observation via Haiku rewrites), never subtracts.

3. **Cross-project promotion requires manual invocation.** No automatic
   background job promotes qualifying instincts; `/promote` must be run
   explicitly. The `/evolve` command only **suggests** candidates
   (`instinct-cli.py:1264-1272`).

4. **The observer is a black-box LLM call.** Haiku decides what to write based
   on a free-form prompt (`observer-loop.sh:153-193`). There's no schema
   validation on the YAML it produces. If Haiku writes malformed YAML,
   `parse_instinct_file()` silently drops the instinct
   (`instinct-cli.py:546-547`).

5. **No deduplication of similar instincts during creation.** Observer prompt
   says "If a similar instinct already exists, update it instead of creating a
   duplicate" but this is a Haiku instruction with no enforcement. Two slightly
   different triggers can produce two instincts that should have been merged.

6. **No search/ranking of evolved artifacts.** Once `_generate_evolved()`
   writes a SKILL.md, there's no signal loop that tracks whether it's actually
   being used. The "evolution" pipeline is write-only — no feedback from
   whether the skill improved anything.

7. **Test coverage is parse-only.** `test_parse_instinct.py` (1045 lines) tests
   YAML parsing, project detection, status display, promotion logic — but does
   **not** test the observer loop, hook pipeline, or actual evolution behavior.

8. **Bash-heavy implementation.** The hook (`observe.sh`) and loop
   (`observer-loop.sh`) are 857 lines of bash with embedded Python via
   heredocs. This is fragile: env var passing, signal handling, Windows path
   quirks (MSYS, `:842`), Python stub detection — all require careful
   workarounds. A pure-Python implementation would be substantially more robust.

9. **Memory location change is a one-way door.** v2.0 data at
   `~/.claude/homunculus/` is migratable, but the migration refuses to run if
   `observer-loop.sh` is alive and refuses if both paths have content
   (`migrate-homunculus.sh:40-48`).

10. **No retention policy on observations.** The hook rotates at 10MB but never
    prunes observations older than 30 days except the `observations-*.jsonl`
    archives (which are kept forever unless manually deleted). For long-lived
    projects, `observations.archive/` grows unbounded.

---

## 10. Improvement Suggestions (Where I'd Push Next)

If I were going to evolve this system, in priority order:

| # | Improvement | Why | Difficulty |
|---|-------------|-----|------------|
| 1 | **Add semantic search over instincts** | Current retrieval is `ls + cat`; with 100+ instincts this breaks down. Use hash-bag or fastembed (already in user's stack per memory) | Medium |
| 2 | **Schema validation on observer output** | Wrap the LLM's instinct YAML in a try/parse loop; reject malformed before write; force regeneration | Low |
| 3 | **Implement confidence decay** | A nightly cron that reads `last_observed` from each instinct and applies `-0.02/week`. Prevents stale-but-high-confidence instincts | Low |
| 4 | **Background promotion daemon** | After every analysis, check `_find_cross_project_instincts()` and auto-promote qualifying instincts. Removes the manual `/promote` step | Medium |
| 5 | **Add a `feedback` mechanism** | When an evolved skill is used, observe whether the user corrects it. Use this to update source instincts' confidence (closes the loop) | High |
| 6 | **Test the evolution pipeline end-to-end** | Current tests are parser-only. Need fixtures with mock observations → mock Haiku output → assert instinct files created correctly | Medium |
| 7 | **Observations retention policy** | Auto-prune `observations.archive/` files older than 90 days | Low |
| 8 | **Replace bash with Python** | The hook + loop are 857 lines of bash that exist because of legacy. A pure-Python observer daemon would eliminate the Windows/MSYS/Python-stub complexity | High (refactor) |

---

## 11. File Reference Index

| File | Purpose | Key Lines |
|------|---------|-----------|
| `SKILL.md` | Activation triggers, command reference, comparison tables | 1-361 |
| `config.json` | Observer config (enabled=false default) | 1-8 |
| `hooks/observe.sh` | PreToolUse/PostToolUse capture | 522 total |
| `hooks/observe.sh:1-126` | stdin read, phase detection, project detection | |
| `hooks/observe.sh:127-182` | Session guards (5 layers) | |
| `hooks/observe.sh:200-360` | JSON parsing + secret scrubbing + write | |
| `hooks/observe.sh:362-473` | Observer lazy-start with locks | |
| `hooks/observe.sh:475-520` | Throttled SIGUSR1 signaling | |
| `agents/observer-loop.sh` | Long-running analysis loop | 335 total |
| `agents/observer-loop.sh:17-19` | Cooldown + idle timeout config | |
| `agents/observer-loop.sh:109-270` | `analyze_observations` (sample, prompt, spawn, archive) | |
| `agents/observer-loop.sh:272-298` | SIGUSR1 handler with re-entrancy guard | |
| `agents/observer.md` | Observer agent instructions + scope decision table | 198 total |
| `agents/observer.md:120-148` | Confidence calculation rules | |
| `agents/start-observer.sh` | Process launcher (start/stop/status/--reset) | 248 total |
| `agents/session-guardian.sh` | Pre-analysis gates (time/cooldown/idle) | 150 total |
| `scripts/detect-project.sh` | Bash project detection + hash computation | 322 total |
| `scripts/lib/homunculus-dir.sh` | XDG dir resolver | 31 total |
| `scripts/instinct-cli.py` | CLI for status/import/export/evolve/promote/projects/prune | 1914 total |
| `scripts/instinct-cli.py:284-394` | `detect_project()` (Python equivalent) | |
| `scripts/instinct-cli.py:471-522` | `parse_instinct_file()` | |
| `scripts/instinct-cli.py:641-673` | `load_all_instincts()` (project + global, dedup) | |
| `scripts/instinct-cli.py:1096-1205` | `cmd_evolve` (cluster analysis) | |
| `scripts/instinct-cli.py:1275-1413` | `cmd_promote` (specific + auto) | |
| `scripts/instinct-cli.py:1614-1685` | `_generate_evolved` (materialize skills/commands/agents) | |
| `scripts/instinct-cli.py:1692-1827` | Pending TTL + prune | |
| `scripts/migrate-homunculus.sh` | v2.0 → v2.1 one-shot path migration | 62 total |
| `scripts/test_parse_instinct.py` | pytest suite (parse, validate, promote, status) | 1045 total |

---

## 12. Conclusion

Continuous Learning v2 is a **reasonably mature** observation-and-pattern-mining
pipeline. The capture layer is solid (deterministic hooks, secret scrubbing,
cross-platform guards). The pattern-detection layer delegates to a background
Haiku agent which is cost-effective but quality-dependent.

**The improvement / evolution mechanism is real but currently disconnected from
feedback.** Instincts evolve into skills via `/evolve --generate`, but there's
no signal flowing back to confirm those skills are actually being applied
correctly. The confidence system is one-directional (set on creation, decay
documented but not run, auto-promotion exists but isn't invoked automatically).

Compared to the user's other memory systems documented in MEMORY.md:
- **vs claude-mem** (Chroma + FTS5 + 3-layer MCP tools): clv2 has **no semantic
  search** — its biggest gap.
- **vs co-scientist** (Turso + inverted index + 3-layer retrieval): clv2 has
  **simpler storage** (YAML files) but **weaker retrieval** (no BM25, no
  embeddings).

The honest take: this is a useful **pattern collector** but not yet a
**learning loop**. Closing that loop (feedback → confidence adjustment →
auto-promotion → re-evaluation) would move it from "observation system" to
"adaptive system."

---

*Generated 2026-06-26. Project: `ref/continuous-learning-v2/`, version 2.1.0.*
