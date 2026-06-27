# MiMo-Code Memory System — Implementation Map

> Detailed report on how the persistent memory system is implemented and integrated across `MiMo-Code/`. Every claim is anchored to a file:line so you can jump straight to the source.
> Scope: `packages/opencode/` (the memory system lives here; the TUI / app / web shells consume it via the SDK).

---

## 0. TL;DR

The memory system is a **four-layer pipeline**:

1. **Files** under `<data>/memory/<scope>/<scope_id>/<key>.md` (`packages/opencode/src/session/checkpoint-paths.ts:15`).
2. **SQLite FTS5 index** in `<data>/mimocode.db` (`packages/opencode/src/memory/fts.sql.ts:3`) with an external-content virtual table (`packages/opencode/migration/20260515010000_memory_fts/migration.sql:15`).
3. **Effect service** `Memory.Service` that reconciles files → index and answers BM25 searches (`packages/opencode/src/memory/service.ts:33`).
4. **Tools + system prompt + checkpoint-writer subagent + rebuild injection** that drive write/read paths (`packages/opencode/src/tool/memory.ts:22`, `packages/opencode/src/session/llm.ts:99`, `packages/opencode/src/session/checkpoint.ts:533`, `packages/opencode/src/session/checkpoint.ts:1041`).

A **sibling history system** (`packages/opencode/src/history/`) keeps raw conversation parts indexed in `history_fts` so the `history` tool can recover verbatim text the curator paraphrased away. Both share the same FTS5 query builder (`packages/opencode/src/memory/fts-query.ts:28` and `packages/opencode/src/history/fts-query.ts`).

---

## 1. Filesystem layout (the on-disk truth)

All paths resolve through helpers in `packages/opencode/src/session/checkpoint-paths.ts`:

| Helper | Path | Purpose |
|---|---|---|
| `metaDir(sessionID)` (`:15`) | `<data>/memory/sessions/<sid>/` | Per-session root |
| `checkpointPath(sessionID)` (`:22`) | `<sid>/checkpoint.md` | v5 single-file checkpoint, 11 sections |
| `memoryPath(projectID)` (`:29`) | `<data>/memory/projects/<pid>/MEMORY.md` | Project-level durable memory |
| `globalMemoryPath()` (`:37`) | `<data>/memory/global/MEMORY.md` | Cross-project user prefs (read-only by default) |
| `notesPath(sessionID)` (`:66`) | `<sid>/notes.md` | Main-agent scratchpad (v8.1) |
| `tasksDir(sessionID)` (`:75`) | `<sid>/tasks/` | Per-task progress journals |
| `progressPath(sessionID, taskID)` (`:84`) | `<sid>/tasks/<TID>/progress.md` | Per-task narrative |
| `migrateProjectMemory(projectID)` (`:48`) | Rename `memory.md` → `MEMORY.md`, atomic, idempotent |

`Global.Path.data` is the platform's per-user data dir (`packages/opencode/src/global.ts`). The memory root is `<Global.Path.data>/memory`.

`<pid>` is `resolveProjectId(absRepoPath)` → first 12 hex chars of `sha256(repoPath)` (`packages/opencode/src/memory/paths.ts:114`).

### Scope taxonomy

`Scope = "global" | "projects" | "sessions" | "cc"` (`packages/opencode/src/memory/paths.ts:4`).

Each file carries an implicit `type` derived from its filename via `detectType()` (`paths.ts:40`):

- `memory.md` / `memory-*.md` → `memory`
- `checkpoint.md` / `checkpoint-*.md` → `checkpoint`
- `tasks/<id>/progress` → `progress`
- `tasks/<id>/notes` → `notes`
- everything else → `free`

`memory.md` matches **case-insensitively** so the index bridges both old and new casings through the migration (`paths.ts:27-28`). The other types are exact-match to prevent silent drift.

`parsePath()` (`:45`) extracts `{scope, scope_id, type, key}` from any path under `<root>/memory/`. `parseCcPath()` (`:59`) handles the Claude Code layout `<…>/.claude/projects/<slug>/memory/<key>.md` (scope = `cc`, scope_id = slug, type deferred to frontmatter parse).

### Path safety

`buildPath()` (`:105`) calls `assertSafeComponent()` (`:96`) which rejects any segment equal to `..` or any path starting with `/`. Tests in `packages/opencode/test/memory/paths.test.ts:166-194` pin this down.

---

## 2. SQLite / FTS5 index

Schema definition: `packages/opencode/src/memory/fts.sql.ts:3`

```ts
export const MemoryFtsTable = sqliteTable("memory_fts", {
  id: integer().primaryKey({ autoIncrement: true }),
  path: text().notNull().unique(),
  scope: text().notNull(),
  scope_id: text().notNull().default(""),
  type: text().notNull(),
  body: text().notNull(),
  fingerprint: text().notNull(),
  last_indexed_at: integer().notNull(),
}, (t) => [
  index("memory_fts_scope_idx").on(t.scope, t.scope_id),
  index("memory_fts_type_idx").on(t.type),
])
```

The companion **FTS5 virtual table** + sync triggers live in a Drizzle migration: `packages/opencode/migration/20260515010000_memory_fts/migration.sql`:

```sql
CREATE VIRTUAL TABLE memory_fts_idx USING fts5(
  body,
  content='memory_fts',
  content_rowid='rowid',
  tokenize='unicode61 remove_diacritics 1'
);
CREATE TRIGGER memory_fts_ai AFTER INSERT ON memory_fts BEGIN
  INSERT INTO memory_fts_idx(rowid, body) VALUES (NEW.rowid, NEW.body); END;
CREATE TRIGGER memory_fts_ad AFTER DELETE ON memory_fts BEGIN
  DELETE FROM memory_fts_idx WHERE rowid = OLD.rowid; END;
CREATE TRIGGER memory_fts_au AFTER UPDATE ON memory_fts BEGIN
  DELETE FROM memory_fts_idx WHERE rowid = OLD.rowid;
  INSERT INTO memory_fts_idx(rowid, body) VALUES (NEW.rowid, NEW.body); END;
```

Pattern: **external-content FTS5** so `memory_fts` holds the body once and `memory_fts_idx` is a tokenized shadow kept in sync by triggers. Updated to v6 schema in migration `20260521010000_memory_fts_v6/` (adds path/scope/scope_id/type/fingerprint columns).

Rowid-stability invariant: the index's `rowid` is pinned to the content table's rowid, so snippets returned by `snippet(memory_fts_idx, …)` are always joinable to the parent row. Tests: `packages/opencode/test/memory/fts-rowid-stability.test.ts`.

Sibling `history_fts` + `history_fts_idx` follow the same shape but index raw conversation parts (`packages/opencode/src/history/fts.sql.ts:3`, migration `20260609000000_history_fts/`).

---

## 3. The `Memory.Service` layer

`packages/opencode/src/memory/service.ts:33` declares an `effect/Context.Service` with three methods:

```ts
interface Interface {
  root(): Effect.Effect<string>
  reconcile(): Effect.Effect<{ indexed: number; pruned: number }>
  search(input): Effect.Effect<{path, snippet, score, scope, scope_id, type}[]>
}
```

### 3.1 `root()` (`service.ts:42`)

Returns `<data>/memory`. Same value used everywhere via `path.join(Global.Path.data, "memory", …)`.

### 3.2 `reconcile()` (`service.ts:46`)

Delegates to `reconcileMemory()` in `packages/opencode/src/memory/reconcile.ts:94`. The pipeline:

1. **Walk** both roots — `<data>/memory/**` and (if `cfg.memory.cc_index`) `~/.claude/projects/<slug>/memory/**` (`reconcile.ts:100-102`).
2. **Build union set** of every disk path. Union *before* pruning because per-root pruning would wipe the other side on a flag flip (`reconcile.ts:97-99`).
3. **Direction B (prune)** — delete FTS rows whose path is no longer on disk (`reconcile.ts:114-120`).
4. **Direction A (index)** — for each file, stat it, compute fingerprint `size-mtimeMs`. If unchanged → `'hit'` (skip), else re-read body, upsert, mark `'updated'` (`reconcile.ts:46-92`). CC files derive `type` from `parseCcFrontmatterType()` (`paths.ts:86`); mimo files keep the path-derived `loc.type`.

Tests cover index/prune/fingerprint/cache hit: `packages/opencode/test/memory/reconcile.test.ts`, `cc-reconcile.test.ts`.

### 3.3 `search()` (`service.ts:52`)

Pipeline:

1. **Lazy reconcile** (if `checkpoint.memory_reconcile_on_search`, default `true`).
2. Build FTS5 MATCH query via `buildFtsQuery(raw)` (`packages/opencode/src/memory/fts-query.ts:28`):
   - Tokenize on `/[\p{L}\p{N}_]+/gu` (Unicode letters + numbers + underscore, so CJK works).
   - Wrap each token in `"…"` (phrase quotes neutralise FTS5 special chars `*`, `:`, `-`, `(`, `)`, `^`, `.`, `{`, `}`).
   - Join with ` OR ` (NOT `AND` — see comment in `fts-query.ts:5-22`: AND-join zeroed recall because any non-matching token dropped the whole doc).
3. Run a raw SQL query against `memory_fts_idx` joined to `memory_fts` with optional `scope`/`scope_id`/`type` filters (`service.ts:102-112`).
4. **Over-fetch** `limit * 3` (capped at 50) so the score floor can trim common-word noise without starving real hits (`service.ts:116`).
5. **Relative score floor**: keep rows scoring ≥ `floorRatio × topHit.score`. The #1 result is always kept. Default 0.15, configurable via `checkpoint.memory_search_score_floor` (`service.ts:130-133`). Relative (not absolute) because BM25 magnitudes collapse on tiny corpora.
6. Negate `bm25()` so the caller sees higher = better (`service.ts:120-127`).

Edge cases tested in `packages/opencode/test/memory/service.test.ts`:
- FTS5-special-char queries (`service.test.ts:101-120`).
- Multi-word OR vs AND semantics (`service.test.ts:122-168`).
- Scope / scope_id / limit filters (`service.test.ts:44-99`).

---

## 4. The `memory` tool (agent-facing read)

`packages/opencode/src/tool/memory.ts:22` defines a Zod-validated tool with one operation today (`search`). Schema (`memory.ts:7-20`):

```ts
{
  operation: "search",
  query: string,
  scope?: "global" | "projects" | "sessions" | "cc",
  scope_id?: string,
  type?: string,    // memory | checkpoint | progress | notes | free | feedback | project | reference | user
  limit?: number
}
```

Execute (`memory.ts:29`) calls `Memory.Service.search` and formats:

- **0 results** → structured escalation hint (`:42-54`) telling the agent to retry with rarer terms, fall back to `Grep` on the memory dir for literal strings, or use `history` for verbatim recall.
- **≥1 results** → ranked list with score, scope/scope_id, type, snippet (`:58-77`).

Description (`packages/opencode/src/tool/memory.txt`) teaches the model:
- Queries are OR-joined BM25 (ranked, not boolean).
- Pick 1-3 distinctive terms.
- A HIT IS AUTHORITATIVE — trust it.
- CC scope is opt-in via `cfg.memory.cc_index`; `type: user` and `type: feedback` may contain personal context CC wrote for itself; flipping the flag exposes them to every mimocode agent.

### Registration

Wired into the tool registry at `packages/opencode/src/tool/registry.ts:229` (`tool.memory`), included in the builtin list (`:257`), and provided into `ToolRegistry.defaultLayer` via `Memory.defaultLayer` (`:421`).

Tests: `packages/opencode/test/tool/memory.test.ts` (search execution), `packages/opencode/test/tool/memory-path-guard.test.ts` (write-side guard), `packages/opencode/test/tool/memory-edit-ask-skip.test.ts` (permission skip for memory paths).

---

## 5. The `memory-path-guard` (write-side authority)

`packages/opencode/src/tool/memory-path-guard.ts:95` is the **only** function allowed to vet writes into `<data>/memory/`. Called from `assertWriteAllowed()` in `packages/opencode/src/tool/external-directory.ts:76`, which in turn is the single write gate for `edit`/`write`/`apply_patch`/`notebook-edit`.

Two policies (`memory-path-guard.ts:120-161`):

- **`agentName === "checkpoint-writer"`** — must hit one of:
  - `projects/<pid>/MEMORY.md` (or `memory-<topic>.md` spillover).
  - `sessions/<sid>/checkpoint.md` (or `checkpoint-<topic>.md` spillover).
  - `sessions/<sid>/notes.md`.
  - `sessions/<sid>/tasks/<TID>/*.md` (any depth).
- **All other agents** — cannot touch `sessions/<sid>/tasks/*`. Exception: if spawned with `task_id = <TID>`, the subagent may write under `tasks/<TID>/` (any depth), so it can keep its own workspace nested (`memory-path-guard.ts:144-152`).

Anything outside `<data>/memory/`, and any free key under a valid scope, passes through unmodified.

Two clarifications live in the file's jsdoc and tests:

- The legacy `pinned.md` name (v4) is rejected; only `memory.md`/`memory-<topic>.md` is allowed (`memory-path-guard.ts:28`).
- The CC scope is not in `VALID_SCOPES` (`memory-path-guard.ts:5`), so the guard explicitly blocks mimocode agents from writing into CC's tree — CC memory is read-only via the `memory` tool.

### Why a separate guard exists

`assertExternalDirectoryEffect` (`external-directory.ts:19`) defers to it for any path under `<data>/memory` (`:37`) — because in headless `mimo run` mode there's no permission replier and asking external_directory would deadlock on a never-resolved Deferred (`external-directory.ts:34-37`). The `edit` permission ask is also skipped for memory paths via `askEditUnlessMemory()` (`:119-131`) for the symmetric reason.

`apply_patch.ts:193,209` and `notebook-edit.ts:208` call the same gate so no write tool can drift into leaving the memory tree unguarded.

---

## 6. The `checkpoint-writer` subagent (curator)

This is how structured memory actually gets *written*. The writer is spawned as a background subagent by `SessionCheckpoint.tryStartCheckpointWriter()` (`packages/opencode/src/session/checkpoint.ts:533`).

### 6.1 Spawn flow (parent → child session)

1. **Trigger source** — `SessionPrompt` calls into `tryStartCheckpointWriter` either:
   - On a context-fill threshold hit (`packages/opencode/src/session/prune.ts:307`).
   - From `insertRebuildBoundary` after a trim (`checkpoint.ts:1358`).
   - At session-end via `drainWriters` (`checkpoint.ts:958`).
2. **Queueing** — if a writer is already running for the session, the new request becomes a 1-slot pending entry; newer requests evict older pending ones (newest range is a strict superset → older one would only duplicate work, F40 spec, `checkpoint.ts:541-551`).
3. **Skip rules** — empty session / system-spawned / `Actor.service` unavailable → `"skipped"` (`checkpoint.ts:557-562, 660`).
4. **Compute boundary** — `computeBoundary()` (`checkpoint.ts:235`) walks back from the last finished assistant turn to accumulate ≥10K tokens and ≥5 text-block messages in the tail (soft cap 20K). Then `adjustBoundaryForApiInvariants()` aligns past tool_use/tool_result pairing.
5. **Path resolution** — absolute paths resolved here because `Instance.current` is ALS-bound and lost once the writer fiber detaches (`checkpoint.ts:609-619`):
   ```
   sessMemDir   = <data>/memory/sessions/<sid>
   projectMemDir= <data>/memory/projects/<pid>
   checkpointFile= checkpointPath(sid)
   memoryFile    = memoryPath(pid)
   taskMemDir    = <sid>/tasks
   notesFile     = notesPath(sid)
   ```
6. **Template bootstrap** — if missing, write `CHECKPOINT_TEMPLATE`/`MEMORY_TEMPLATE`/`NOTES_TEMPLATE` from `packages/opencode/src/session/checkpoint-templates.ts`.
7. **Migration** — `migrateProjectMemory(projectID)` (`checkpoint-paths.ts:48`) renames any legacy `memory.md` → `MEMORY.md`.
8. **Prefix-capture fork context** — `prefixCaptureRef` (a late-bound closure in `SessionPrompt.layer`, see `prefix-capture-ref.ts`) freezes the parent's system+tools+messages at the watermark, so prefix-cache alignment survives. Axis B (`checkpoint.fork`, default `false`) toggles parent-fork vs cold-start delta (`checkpoint.ts:664-815`).
9. **Spawn child session** — `session.create({parentID, title: "checkpoint-writer: …"})` (`checkpoint.ts:827`). Axis A: writer always runs in a fresh child session so the parent's messages and actor registry stay clean.
10. **Spawn actor** — `actor.spawn({mode: "subagent", agentType: "checkpoint-writer", parentSessionID, tools: ["read","write","edit","apply_patch","glob","grep","task"], background: true, forkContext})` (`checkpoint.ts:844`).
11. **Register CheckpointContext** — `CheckpointContext.set(sessionID, actorID, {priorTitles, expectedRevisions: []})` BEFORE the writer fires so the splitover plugin's preStop hook can read it (`checkpoint.ts:875-879`).
12. **Bookkeeping fiber** — forks into the layer's scope, awaits the writer's `Deferred<AgentOutcome>`, advances `session.last_checkpoint_message_id` to `endMessageID`, drains any pending queued request, and emits `WriterCachePerf` metrics (`checkpoint.ts:887-938`).

### 6.2 The writer's prompt

`packages/opencode/src/agent/prompt/checkpoint-writer.txt` is the system prompt. Sections worth noting:

- **11-section checkpoint structure** (§1 Active intent … §11 Open notes) — `templates.ts:1-58`.
- **§4 Task tree** must be pulled from the `task` tool's DB, never invented (`:73-79`).
- **§1 commitment vs inspection update policy** — block-quoted verbatim user request; update only on COMMITMENT-style verbs (`implement`, `build`, `fix`, …), preserve on INSPECTION-style (`find`, `list`, `show`, …). Default to KEEP when unsure (`:124-135`).
- **EXACT-FORM CONSTRAINT LITERAL rule** — connection strings, ports, env vars, full command lines, IDs, seeds, version pins: copy VERBATIM into §3 (session) or MEMORY.md ## Rules (project-durable). The writer's jsdoc at `templates.ts:54-57` enumerates this.
- **notes.md lifecycle** — read in Turn 1, consider every entry in Turn 2a reconcile, then in Turn 2's Edit pass **overwrite notes.md with NOTES_TEMPLATE byte-for-byte** (`checkpoint-writer.txt:114`). The agent re-appends fresh entries in subsequent turns.
- **Spillover rule** — if a section approaches `CHECKPOINT_SECTION_BUDGETS` (`templates.ts:88-100`), extract a coherent topic to `checkpoint-<topic>.md` and replace with `- See checkpoint-<topic>.md (N items) - <summary>` (`:144-150`).

### 6.3 Tool whitelist + memory-path-guard intersection

The writer's tools are restricted to `read, write, edit, apply_patch, glob, grep, task` (`checkpoint.ts:859`). The `memory-path-guard` then restricts WHERE those tools may write — only the canonical writer paths.

### 6.4 Splitover plugin (the writer's validator)

`packages/opencode/src/plugin/checkpoint-splitover.ts:16` registers a plugin hook (`actor.preStop` for `agentType: "checkpoint-writer"`) that:

1. Resolves parent sessionID (`input.parentSessionID ?? input.sessionID`) because the writer lives in a child session post-Axis-A.
2. Pulls `CheckpointContext` (`packages/opencode/src/session/checkpoint-context.ts:8`, a per-session+actor Map).
3. Calls `runValidatorsForCkpt()` from `checkpoint-retry.ts` to check title extraction, section budget violations, structural invariants.
4. On `severity: "extract-required"` → `output.continue = true; output.reason = buildExtractionReflection(…)` — loops the writer back for an extraction pass.
5. On `severity: "error"` → sets a reflection reason, loops the writer.
6. `warn-only` falls through silently.

This is what makes `CHECKPOINT_SECTION_BUDGETS` enforceable: budget overruns become a validator violation, validator violation becomes a retry reason.

### 6.5 Writer failure semantics

`SessionCheckpoint` retries failed writers up to `cfg.checkpoint.max_writer_failures` (default 3) per session; exceeding clears the session's crossed thresholds and stops retrying until restart (`prune.ts:294-349`). The `WriterCachePerf` event (`packages/opencode/src/actor/events.ts`) lets `mimo` observe prefix-cache hit rates per writer fire.

---

## 7. Rebuild / context-injection path

When context needs to be rebuilt (compaction), `SessionCheckpoint.renderRebuildContext()` (`checkpoint.ts:1041`) produces a self-contained markdown block that gets inserted as a synthetic user message after the boundary. The block has 9 sections:

| # | Section | Source | Cap (default) |
|---|---|---|---|
| Header | "already loaded" notice (anchors Active recall) | inline | — |
| 3 | `## Tasks ledger` | `taskRegistry.list({session_id, include_terminal:true})` | 2000 tok |
| 5 | `## Session checkpoint` | `readBudgetedSectionAware(checkpointPath(sid))` | 11000 |
| 6 | `## Active actors` | `actorRegistry.listActive()` | 500 |
| 6.5 | `## Recent user input (verbatim)` | `MessageV2.page(...).items` filtered for user role, FIFO-bounded, `truncateVerbatimUserMsg` caps each at 2000 tok | 16000 |
| 7 | `## Project memory` | `readBudgetedSectionAware(memoryPath(pid))` | 10000 |
| 7.4 | `## Global memory` | `readBudgetedSectionAware(globalMemoryPath())` | 6000 |
| 7.5 | `## Session notes` | `readBudgeted(notesPath(sid))` | 6000 |
| 8 | `## Memory keys index` | SQL query against `memory_fts` scoped to (current project OR current session OR global), excluding already-pushed paths and `checkpoint/learning-*` | 500 |
| 10 | Resume framing | inline | — |
| 11 | Tail-aware reminder | `lastMessageInfo` switch (`autonomousLoopReminder` / `stopReminder` / `toolResultContinueReminder`) | — |

All caps configurable via `cfg.checkpoint.push_caps.*` (`checkpoint.ts:1069`).

`readBudgetedSectionAware()` (`packages/opencode/src/session/budgeted-read.ts:65`) parses `## section` headers + italic instructions, fits as many complete sections as the budget allows, then truncates the last one and emits a `⚠️ Truncated at ~N tokens. Read(path) for the rest.` hint. Spillover index lines (`- See foo.md (N items) …`) are preserved across all rebuilds because they're a stable map.

### Why Section 8 (memory keys index) matters

This is the FTS index exposed in the rebuilt context. Before query, the code calls `memory.reconcile()` so off-tool writes (e.g. by the writer subagent) are visible (`checkpoint.ts:1257`). Scoping to `currentProjectID OR sessionID OR global` prevents leaking other sessions' files.

### Synthetic boundary insertion

`insertRebuildBoundary()` (`checkpoint.ts:1358`) writes the rebuild context as a synthetic user message with `type: "checkpoint"` part + `synthetic: true` text parts. The `userMsgText()` filter (`checkpoint.ts:76`) and the `m.parts.some(p => p.type === "checkpoint")` filter (`:1131`) together ensure the next rebuild doesn't re-ingest the prior one (fractal bloat prevention).

### Tail microcompact

After the boundary is inserted, `insertRebuildBoundary()` (`:1446-1470`) walks every message strictly newer than the boundary and, for completed tool parts in `COMPACTABLE_TOOL_NAMES` (read, bash, grep, glob, webfetch, websearch, edit, write, multiedit, apply_patch, codesearch), sets `state.time.compacted = Date.now()`. The downstream tool converter turns these into `[Old tool result content cleared]` placeholders so the first uncached request after rebuild is smaller.

---

## 8. The agent's view of the memory system

`buildMemoryInstructions()` in `packages/opencode/src/session/llm.ts:99` appends a `# Memory system` block to every main-agent system prompt (system-spawned actors like the writer skip it, see `llm.ts:263-265`).

The block teaches the agent:

- The 4 file types and their absolute paths (computed from `memory.root()` so the path it advertises matches the path the writer actually writes).
- **"When to Edit MEMORY.md directly"** — only on project-level user-stated rules, arch decisions, or clearly-durable facts (`llm.ts:115-122`). Otherwise let the writer handle it.
- **Notes scratchpad** — `notes.md` is the ONLY legal scratchpad; never create `learning.md`/`scratch.md` (`llm.ts:124-137`).
- **Subagent return format** — `Status / Summary / deliverable / Files touched / Findings` (`llm.ts:139-151`).
- **What NOT to do** — don't Edit `checkpoint.md`; don't create ad-hoc memory files; don't ask the user something memory may already record (`llm.ts:153-157`).
- **Active recall protocol** — after a rebuild, the dumps are already in context; don't re-Read; use Grep for specific facts; use Read with offset for truncated dumps (`llm.ts:159-178`).

The prompt also bootstraps `migrateProjectMemory(projectID)` once at session start (`:278`) so a legacy lowercase file is renamed before the agent's first direct Edit/Write.

---

## 9. Config surface

`packages/opencode/src/config/config.ts:282-348` exposes:

```ts
checkpoint?: {
  // Token-fill thresholds that fire tryStartCheckpointWriter.
  thresholds: string[]            // default ["40%","60%","80%"]
  reserved: number                // default 20000
  max_writer_failures: number     // default 3

  // Rebuild-context caps.
  push_caps?: {
    checkpoint: number           // 11000
    memory: number               // 10000
    global: number               // 6000
    notes: number                // 6000
    recent_user: number          // 16000
    recent_user_per_msg: number  // 2000
    tasks_ledger: number         // 2000
    actor_ledger: number         // 500
    memory_titles: number        // 500
    ...section-budget overrides
  }

  // FTS behaviour.
  memory_reconcile_on_search: boolean // default true
  memory_search_score_floor: number   // default 0.15

  // Spawn mode.
  fork: boolean                       // default false (cold-start delta)
}

memory?: {
  cc_index: boolean                   // default false (opt-in Claude Code recall)
}

dream?:   { auto: boolean, interval_days: number }   // default true, 7
distill?: { auto: boolean, interval_days: number }   // default true, 30
```

These fields propagate into the SDK via `packages/sdk/js/src/v2/gen/types.gen.ts:2135-2240` (Zod schema mirrors).

---

## 10. Dream + Distill (background consolidation)

### Auto-dream (`packages/opencode/src/session/auto-dream.ts`)

Spawned by `SessionPrompt` at `step === 1` of the first non-child session (`packages/opencode/src/session/prompt.ts:2548-2581`). `shouldAutoDream(cfg)` (`:103`) checks:

1. `cfg.dream?.auto !== false` (enabled).
2. Last "Auto Dream" session ≥ `cfg.dream?.interval_days` (default 7d) ago.
3. If never run, project must be ≥ interval old (otherwise skip — nothing to consolidate yet).
4. `MIN_SPAWN_GAP_MS = 10s` rate-limit.

On trigger, creates a fresh session titled `Auto Dream` and prompts it with `DREAM_TASK` (`:20`) and agent `dream`. Distill mirrors the same shape with 30-day default and `DISTILL_TASK`.

### Manual `/dream` prompt (`packages/opencode/src/agent/prompt/dream.txt`)

Five phases (Locate → Orient → Gather → Verify → Consolidate → Prune). The agent reads memory files, runs read-only SQLite queries against `<data>/mimocode.db`, then edits project `MEMORY.md` (sections: Rules / Architecture decisions / Discovered durable knowledge / Patterns / Gotchas). Hard limits: ≤200 lines, ≤10KB. Stale entries get pruned; unverifiable ones tagged `[unverified]`. Source session IDs preserved at line ends as `[ses_xxx]`.

### Manual `/distill` prompt (`packages/opencode/src/agent/prompt/distill.txt`)

Reviews ~30 days of sessions, identifies repeated manual workflows, packages high-confidence candidates into reusable skills / subagents / commands. Reuses existing assets instead of duplicating.

---

## 11. The sibling `history` system (verbatim recall)

Why does it exist? The memory tools paraphrase by design — the writer is told to keep checkpoint.md under `CHECKPOINT_SECTION_BUDGETS`. So when an agent needs the **literal byte** of a connection string, port, or command line, memory's snippets lose precision. The `history` tool (`packages/opencode/src/tool/history.ts:42`) recovers it from raw conversation parts.

### Index

`packages/opencode/src/history/fts.sql.ts:3` — `history_fts(part_id PK, session_id, message_id, project_id, kind, tool_name, body, time_created)`. Plus `history_fts_idx` external-content FTS5 (created in migration `20260609000000_history_fts/`). Three secondary indexes for the typical query shapes (`session_id+time_created`, `project_id+time_created`, `message_id`).

### Ingestion

A `Writer` service (`packages/opencode/src/history/writer.ts`) subscribes to `Bus` events from `Backfill` (`:1`) and incrementally indexes new parts as they're persisted. `Backfill` (`packages/opencode/src/history/backfill.ts`) handles initial population from existing messages.

### Search + around

`History.Service.search()` (`packages/opencode/src/history/service.ts:86`) — same `buildFtsQuery` tokenizer as memory, scope defaults to `project`, supports `kind` / `tool_name` / `time_after` / `time_before` filters. Hard cap 50 results.

`History.Service.around()` (`:150`) — given a `message_id`, fetches ±N (default 5/5) messages from the same session ordered by `(time_created, id)` then joins parts. Returns `{matched, role, type, tool_name, text}` per message with a 20KB truncation cap (`tool/history.ts:23`).

### Description for the agent

`packages/opencode/src/tool/history.txt` explains the search→around→targeted-Read escalation path. The `memory` tool itself nudges the agent toward `history` when a memory hit paraphrases a literal (`tool/memory.ts:62`).

---

## 12. End-to-end data flow

```
                       ┌─────────────────────────────────────────────────────────┐
                       │ USER PROMPT arrives at SessionPrompt.runLoop            │
                       │ (packages/opencode/src/session/prompt.ts:2548)         │
                       └─────────────────────────────────────────────────────────┘
                                          │
                                          ▼
        ┌──────────────────────────────────────────────────────────────────────────┐
        │ 1. LLM streaming. Agent invokes tools, accumulates tokens.              │
        │    Memory is untouched — only the History writer backfills parts.       │
        └──────────────────────────────────────────────────────────────────────────┘
                                          │
                                          ▼
       ┌──────────────────────────────────────────────────────────────────────────┐
       │ 2. prune.ts:fireCheckpoints detects threshold cross (e.g. 40%).         │
       │    → SessionCheckpoint.tryStartCheckpointWriter                         │
       │       (packages/opencode/src/session/checkpoint.ts:533)                  │
       └──────────────────────────────────────────────────────────────────────────┘
                                          │
                                          ▼
       ┌──────────────────────────────────────────────────────────────────────────┐
       │ 3. Compute boundary; build ForkContext (system+tools+inheritedMsgs).    │
       │    Resolve projectID from Instance; ensure dirs exist;                  │
       │    bootstrap templates; migrateProjectMemory(pid).                      │
       │    Spawn child session + checkpoint-writer actor with                   │
       │    tools whitelist + parentSessionID + memory-path-guard implicit.      │
       └──────────────────────────────────────────────────────────────────────────┘
                                          │
                                          ▼
       ┌──────────────────────────────────────────────────────────────────────────┐
       │ 4. Writer reads CHECKPOINT_PATH/MEMORY_PATH/NOTES_PATH in parallel.     │
       │    Pulls task list via task tool. Reconciles notes.md entries           │
       │    into §3/§7/§10 of checkpoint.md and Rules/Discovered/Arch in MEMORY.md│
       │    Writes checkpoint.md, MEMORY.md as needed. Overwrites notes.md       │
       │    with NOTES_TEMPLATE. Spillovers go to checkpoint-<topic>.md          │
       │    if a section exceeds CHECKPOINT_SECTION_BUDGETS.                     │
       │    All writes validated by memory-path-guard.                           │
       └──────────────────────────────────────────────────────────────────────────┘
                                          │
                                          ▼
       ┌──────────────────────────────────────────────────────────────────────────┐
       │ 5. actor.preStop (CheckpointSplitoverPlugin) runs the writer's          │
       │    output through validators (title extraction, section budgets,        │
       │    structural invariants). On violation → continue=true + reflection    │
       │    message loops the writer back for an extraction pass.                │
       └──────────────────────────────────────────────────────────────────────────┘
                                          │
                                          ▼
       ┌──────────────────────────────────────────────────────────────────────────┐
       │ 6. Writer settles. Watcher advances                                    │
       │    session.last_checkpoint_message_id = endMessageID.                   │
       │    If a queued request waited, fire a fresh writer for it (F40).        │
       │    Emit WriterCachePerf metric.                                         │
       └──────────────────────────────────────────────────────────────────────────┘
                                          │
                                          ▼
       ┌──────────────────────────────────────────────────────────────────────────┐
       │ 7. Context fills again. Compact path calls                              │
       │    SessionCheckpoint.insertRebuildBoundary → renderRebuildContext.      │
       │    Reconcile memory FTS index (covers off-tool writes by writer).       │
       │    Render 9-section dump (Tasks / Checkpoint / Actors / Recent user /   │
       │    Project memory / Global memory / Notes / Memory keys index / framing).│
       │    Insert synthetic user message after boundary.                       │
       │    Microcompact tool_results for compactable tools strictly newer than  │
       │    the boundary.                                                        │
       └──────────────────────────────────────────────────────────────────────────┘
                                          │
                                          ▼
       ┌──────────────────────────────────────────────────────────────────────────┐
       │ 8. Next LLM stream sees the rebuilt context. Active recall protocol     │
       │    instructs the agent NOT to re-Read the dumped files; use Grep for    │
       │    facts; use history tool for verbatim; use memory search for new       │
       │    lookups against the now-up-to-date FTS index.                        │
       └──────────────────────────────────────────────────────────────────────────┘
```

### Auto-dream side channel (parallel)

At `step === 1` of a fresh top-level session, `shouldAutoDream`/`shouldAutoDistill` fire (every 7d/30d). A new `Auto Dream` session is created and the `dream` agent reads memory + raw trajectory + edits `MEMORY.md` to consolidate durable knowledge.

---

## 13. Tests & testability

Per `packages/opencode/test/AGENTS.md`, tests use `testEffect(Layer.mergeAll(Memory.defaultLayer, CrossSpawnSpawner.defaultLayer))` + `provideTmpdirInstance(...)`.

### Memory unit + integration (`packages/opencode/test/memory/`)

| File | Covers |
|---|---|
| `paths.test.ts` | `parsePath`/`buildPath`/`resolveProjectId` — including legacy `pinned.md` rejection, case-insensitive `MEMORY.md`, multi-segment keys, path-traversal rejection. |
| `reconcile.test.ts` | Index, prune, fingerprint hit, reindex on content change. |
| `service.test.ts` | BM25 ranking, scope/scope_id filters, FTS5 special chars, OR semantics, empty query. |
| `cc-reconcile.test.ts` | CC scope (`~/.claude/projects/<slug>/memory`), frontmatter-derived types, union prune across roots, flag on→off prunes CC rows, ENOENT silent. |
| `cc-frontmatter.test.ts` | YAML frontmatter parsing for `metadata.type`. |
| `cc-paths.test.ts` | `parseCcPath` regex behaviour. |
| `cc-search.test.ts` | CC scope search integration. |
| `fts-rowid-stability.test.ts` | `memory_fts.rowid == memory_fts_idx.rowid` invariant. |
| `fts-query.test.ts` | Tokenizer, CJK handling, OR/AND, special-char neutralisation. |
| `abort-leak.test.ts`, `abort-leak-webfetch.ts` | Failure-mode coverage. |

### Tool guards (`packages/opencode/test/tool/`)

| File | Covers |
|---|---|
| `memory.test.ts` | `MemoryTool` execution (search, no-results message). |
| `memory-path-guard.test.ts` | Allowlist for `checkpoint-writer`; rejection of legacy `pinned.md`; `tasks/<TID>/*` allow for task-bound subagent; cross-task writes rejected. |
| `memory-edit-ask-skip.test.ts` | `edit` permission ask is skipped for memory paths. |
| `whitelist.test.ts` | Per-agent tool allowlist. |
| `external-directory.test.ts` | Memory subtree is deferred to `memory-path-guard`. |
| `apply_patch.test.ts` | `apply_patch` rejects cross-task memory writes. |

### Checkpoint subsystem (`packages/opencode/test/session/`)

The full `checkpoint-*` suite covers boundary computation, validator/splitover integration, fork-mode, drain semantics, rebuild-context section ordering, threshold triggers, permission flows, retry logic. Notable files: `checkpoint-rebuild-unify.test.ts`, `checkpoint-rebuild-v3.test.ts`, `checkpoint-render-verify.test.ts`, `checkpoint-splitover-integration.test.ts`, `checkpoint-permission.test.ts`, `checkpoint-child-session.test.ts`, `checkpoint-fork-mode.test.ts`, `checkpoint-main-slice.test.ts`, `checkpoint-progress-reconcile.test.ts`.

### History (`packages/opencode/test/history/`)

`backfill.test.ts`, `extract.test.ts`, `fts-query.test.ts`, `resolve.test.ts`, `service.test.ts`, `writer.test.ts` — all sibling-tests for `history_fts`.

---

## 14. Cross-system guarantees the design enforces

| Invariant | How it's enforced |
|---|---|
| A checkpoint write can never happen outside the canonical writer paths. | `memory-path-guard` allowlist + `assertWriteAllowed` is the single write gate (external-directory.ts:76). |
| The agent never Edit's `checkpoint.md` mid-session. | System prompt's "What NOT to do" section + `memory-path-guard` rejects non-writer writes to `checkpoint.md`. |
| `notes.md` never accumulates indefinitely. | Writer overwrites with `NOTES_TEMPLATE` on every checkpoint event (`checkpoint-writer.txt:114`). |
| `MEMORY.md` stays under 200 lines / 10 KB. | Manual `/dream` enforces (dream.txt:137); no automatic cap yet, but writer's reconciliation extracts durable items aggressively. |
| Off-tool writes (by the writer subagent) are visible to memory search. | Lazy `reconcile()` at the top of every `search()` (service.ts:61) and in `renderRebuildContext` (checkpoint.ts:1257). |
| Path traversal is impossible from any caller-supplied `scope_id` or `key`. | `assertSafeComponent()` in `paths.ts:96` rejects `..` and leading `/`. |
| The rebuild context never re-ingests itself (fractal bloat). | Synthetic messages tagged `type: "checkpoint"` + `synthetic: true` are filtered at read time (`checkpoint.ts:1131`, `userMsgText` `:76`). |
| CC memory is read-only from mimocode agents. | `VALID_SCOPES` in `memory-path-guard.ts:5` excludes `cc`; CC paths simply do not match the `parsePath` regex and are invisible to the guard. |
| Personal CC contexts (`type: user`, `type: feedback`) are off by default. | `cfg.memory.cc_index` defaults to `false`; doc warns explicitly (`memory.txt:54-69`, `config.ts:331-335`). |
| Per-section budgets are real, not aspirational. | `CHECKPOINT_SECTION_BUDGETS` enforced by `SplitoverPlugin` via `runValidatorsForCkpt`; over-budget sections become retryable violations. |
| Verbatim user quotes survive the writer's paraphrase. | `## Recent user input (verbatim)` section in rebuild context pulls directly from `MessageV2` (live DB), FIFO-bounded, head+tail trimmed with elision marker (`checkpoint.ts:1115-1144`, `truncateVerbatimUserMsg` `:56`). |
| Literal byte literals (DSNs, ports, command lines) survive in MEMORY.md. | `EXACT-FORM CONSTRAINT LITERAL` rule in the writer prompt (`checkpoint-writer.txt:54-57`) plus the "exact-form" section in templates.jsdoc. |
| The writer's child session never pollutes the parent's message table or actor registry. | Axis A — fresh `session.create({parentID})` per writer fire (`checkpoint.ts:827`); `parentSessionID` propagated to `actor.spawn` so file paths still target parent artifacts. |

---

## 15. Where to start reading

If you want to *understand* the system end-to-end, read in this order:

1. `packages/opencode/src/session/checkpoint-paths.ts` — the on-disk shape (≤ 86 lines).
2. `packages/opencode/src/memory/paths.ts` — how paths are classified.
3. `packages/opencode/migration/20260515010000_memory_fts/migration.sql` — the FTS5 schema.
4. `packages/opencode/src/memory/service.ts` — the search/reconcile pipeline.
5. `packages/opencode/src/tool/memory.ts` + `memory.txt` — the agent-facing read API.
6. `packages/opencode/src/tool/memory-path-guard.ts` — the write authority.
7. `packages/opencode/src/session/checkpoint.ts` (read top-down, skim middle) — the orchestration.
8. `packages/opencode/src/agent/prompt/checkpoint-writer.txt` — the curator's brief.
9. `packages/opencode/src/plugin/checkpoint-splitover.ts` + `packages/opencode/src/session/checkpoint-retry.ts` — the validator feedback loop.
10. `packages/opencode/src/session/llm.ts:99` (`buildMemoryInstructions`) — what the model sees.

If you want to *change* the system, the highest-leverage points are:

- **Cap a different section** → `CHECKPOINT_SECTION_BUDGETS` (`checkpoint-templates.ts:88`) + `push_caps` override (`config.ts:282-298`).
- **Add a memory scope** → extend `Scope` and `parsePath` regex (`memory/paths.ts:4,46`), add to `VALID_SCOPES` in `memory-path-guard.ts:5`, add a path helper in `checkpoint-paths.ts`, add a section to `renderRebuildContext` (`checkpoint.ts:1168-1296`).
- **Change recall ranking** → `buildFtsQuery` (`memory/fts-query.ts:28`) + score floor logic (`memory/service.ts:130-133`).
- **Make the writer stricter** → add a validator in `checkpoint-retry.ts`, return a higher `severity` from `runValidatorsForCkpt`. The splitover plugin picks it up automatically.
- **Add a new agent-observable file type** → register in `detectType` (`memory/paths.ts:40`), decide whether to expose to rebuild context (`renderRebuildContext`), decide whether `memory-path-guard` should permit/deny writes by which agent.

---

## 16. Open edges / not yet integrated

- **Global memory writes** — `globalMemoryPath()` resolves the path, but there's no UI/CLI to author `global/MEMORY.md`. It's read-only by default (system prompt + `cfg` permission); updates land via the writer when a cross-project rule surfaces.
- **Cross-session spillover** — `<data>/memory/projects/<pid>/` allows free `*.md` files, but no tool surfaces them as a category distinct from project memory.
- **FTS5 synonym / trigram support** — `tokenize='unicode61 remove_diacritics 1'` is conservative. CJK recall is enabled via the regex but the tokenizer doesn't segment; multi-char CJK words tokenize char-by-char.
- **Memory key namespaces** — `key` is free-form and may contain `/` (e.g. `tasks/T1/notes`). FTS matches by body, not key, so this is purely a path-routing convention.
- **Dream scheduling per-project vs per-user** — `shouldAutoDream` keys off `SessionTable.title = AUTO_DREAM_TITLE` (the *session title*), not project. If two projects share a machine, the gate runs at machine scope.

---

## 17. Glossary of file types

| Path | Scope_id | Type | Owner | Read by | Write by |
|---|---|---|---|---|---|
| `global/MEMORY.md` | `""` | memory | user | rebuild context, memory search | user (manual) |
| `projects/<pid>/MEMORY.md` | pid | memory | checkpoint-writer + agent | rebuild context, memory search, writer | checkpoint-writer + main agent |
| `projects/<pid>/memory-<topic>.md` | pid | memory | checkpoint-writer | memory search | checkpoint-writer |
| `projects/<pid>/<free>.md` | pid | free | any | memory search | agent (free key) |
| `sessions/<sid>/checkpoint.md` | sid | checkpoint | checkpoint-writer | rebuild context, memory search | checkpoint-writer |
| `sessions/<sid>/checkpoint-<topic>.md` | sid | checkpoint | checkpoint-writer | memory search | checkpoint-writer |
| `sessions/<sid>/notes.md` | sid | notes | main agent | rebuild context | main agent (free-form) |
| `sessions/<sid>/tasks/<TID>/progress.md` | sid | progress | subagent (spec ②) + splitover | writer (reconcile), rebuild | subagent for its own TID |
| `sessions/<sid>/tasks/<TID>/notes.md` | sid | notes | subagent | writer | subagent for its own TID |
| `~/.claude/projects/<slug>/memory/*.md` | slug | free / feedback / project / reference / user | Claude Code (external) | memory search (opt-in) | Claude Code |

---