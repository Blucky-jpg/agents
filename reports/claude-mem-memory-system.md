# claude-mem Memory System — Architecture Map

> Detailed report on how the persistent memory system is implemented and wired together inside `ref/claude-mem/` (version 13.4.0). Every claim is anchored to a file:line so you can jump straight to the source. Read-only investigation — no project files were modified.

---

## 0. TL;DR

claude-mem is a **Claude Code plugin** that gives the agent persistent memory across sessions. The system is a **five-layer pipeline**:

1. **Hooks** declared in `plugin/hooks/hooks.json` capture Claude Code lifecycle events (`SessionStart`, `UserPromptSubmit`, `PostToolUse`, `PreToolUse:Read`, `Stop`) and shell into `plugin/scripts/worker-service.cjs hook <platform> <event>` (`plugin/hooks/hooks.json:18-80`).
2. **CLI dispatch layer** in `src/cli/handlers/*` and `src/cli/adapters/*` normalizes the platform-specific payload into a uniform shape, then makes HTTP calls to the worker daemon (`src/cli/handlers/observation.ts:42`, `src/cli/adapters/claude-code.ts:9-26`).
3. **Worker daemon** — a long-lived Node/Bun process on `127.0.0.1:37777` (`src/shared/SettingsDefaultsManager.ts` DEFAULTS, `src/services/server/Server.ts:212`). Each `POST /api/sessions/observations` pushes a tool event into an in-RAM `SessionMessageBuffer` (`src/services/worker/SessionMessageBuffer.ts:42`), which a per-session generator drains and feeds to the **Claude Agent SDK** (`src/services/worker/ClaudeProvider.ts:178-457`).
4. **Storage** — the SDK is told to emit `<observation>...</observation>` / `<summary>...</summary>` XML; `parseAgentXml()` (`src/sdk/parser.ts:41`) parses it and the rows are written into **SQLite** at `~/.claude-mem/claude-mem.db` (`src/services/sqlite/schema.sql`), deduplicated by `UNIQUE(memory_session_id, content_hash)` (`schema.sql:80`). Fire-and-forget **Chroma** sync mirrors each row into a per-project collection for semantic search (`src/services/sync/ChromaSync.ts:97-154`).
5. **Recall** — on the next `SessionStart` hook the CLI calls `GET /api/context/inject`, the worker queries observations+summaries via `ContextBuilder` (`src/services/context/ContextBuilder.ts:162`), renders a markdown timeline, and emits it as `hookSpecificOutput.additionalContext` — Claude Code feeds it back to the model as pre-prompt context.

A parallel **MCP server** (`plugin/scripts/mcp-server.cjs`, source `src/servers/mcp-server.ts`) exposes `search`, `timeline`, `get_observations`, plus corpus/smart_* tools to the model as MCP primitives. **OpenCode** and **Cursor** get thinner integrations via `src/integrations/opencode-plugin/` and `cursor-hooks/` respectively. A **Postgres-backed "server-beta" runtime** exists behind `CLAUDE_MEM_RUNTIME=server-beta` (`src/services/hooks/runtime-selector.ts:33`) with its own schema in `src/storage/postgres/schema.ts`.

---

## 1. Filesystem layout (where things live)

### Source vs shipped plugin

| Location | What | Ref |
|---|---|---|
| `src/` | TypeScript source | `CLAUDE.md:13` |
| `plugin/` | Built/shipped plugin (consumed by Claude Code) | `CLAUDE.md:14` |
| `~/.claude/plugins/marketplaces/thedotmack/` | Installed marketplace plugin | `CLAUDE.md:15` |
| `~/.claude-mem/claude-mem.db` | SQLite database (WAL mode) | `src/shared/paths.ts:51` |
| `~/.claude-mem/chroma/` | Chroma persistent store | `src/shared/paths.ts:123` |

### Runtime directory (`~/.claude-mem`)

All paths defined in `src/shared/paths.ts`:

| Helper | Path | Ref |
|---|---|---|
| `DATA_DIR` | `$CLAUDE_MEM_DATA_DIR` or `~/.claude-mem` | `paths.ts:18-40` |
| `DB_PATH` | `claude-mem.db` (SQLite, WAL) | `paths.ts:51` |
| `VECTOR_DB_DIR` | `vector-db` | `paths.ts:52` |
| `paths.chroma()` | `chroma/` | `paths.ts:123` |
| `paths.workerPid()` | `worker.pid` | `paths.ts:117` |
| `paths.settings()` | `settings.json` | `paths.ts:121` |
| `LOGS_DIR` / `ARCHIVES_DIR` / `TRASH_DIR` / `BACKUPS_DIR` / `MODES_DIR` | `logs/`, `archives/`, `trash/`, `backups/`, `modes/` | `paths.ts:45-49` |
| `MARKETPLACE_ROOT` | `~/.claude/plugins/marketplaces/thedotmack` | `paths.ts:43` |
| `paths.envFile()` | `.env` | `paths.ts:129` |
| `paths.transcriptsConfig()` / `paths.transcriptsState()` | `transcript-watch.json` / `transcript-watch-state.json` | `paths.ts:125-126` |
| `paths.corpora()` | `corpora/` | `paths.ts:127` |

The Chromium-cert bundle `combined_certs.pem` (`paths.ts:124`) and `.env` (`paths.ts:129`) live next to the DB so the worker is fully self-contained.

### Boot pragmas (`src/services/sqlite/SessionStore.ts:43-46`)

```sql
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
PRAGMA foreign_keys=ON;
PRAGMA journal_size_limit=4194304;
```

Current schema version is `SERVER_STORAGE_SCHEMA_VERSION = 33` (`src/services/sqlite/schema.ts:5`).

---

## 2. SQLite storage schema (`src/services/sqlite/schema.sql`)

The authoritative schema lives in `src/services/sqlite/schema.sql` (182 lines) and is bootstrapped via inline migrations in `SessionStore`'s constructor. Comment at `schema.sql:1-15` confirms it is the **source of truth** and that the durable `pending_messages` table is now mostly superseded by the in-RAM `SessionMessageBuffer` (see §6).

### Tables

| Table | Purpose | Key columns / constraints |
|---|---|---|
| `schema_versions` | Migration bookkeeping | `version INTEGER UNIQUE NOT NULL` |
| `sdk_sessions` | One row per Claude/Codex session | `content_session_id TEXT UNIQUE`, `memory_session_id TEXT UNIQUE`, `project`, `platform_source DEFAULT 'claude'`, `user_prompt`, `status CHECK(active/completed/failed)`, `worker_port`, `prompt_counter`, `custom_title` (6 indexes at `schema.sql:43-48`) |
| `observations` | **Central memory output** — compressed rows | `memory_session_id`, `project`, `text`, `type`, `title`, `subtitle`, `facts JSON`, `narrative`, `concepts JSON`, `files_read JSON`, `files_modified JSON`, `prompt_number`, `discovery_tokens`, `content_hash`, `agent_type`, `agent_id`, `merged_into_project`, `generated_by_model`, `metadata`, `created_at_epoch`. **Dedup key: `UNIQUE(memory_session_id, content_hash)`** (`schema.sql:80`) + 8 indexes (`:82-89`) |
| `session_summaries` | Per-session synthesis (Stop hook) | `memory_session_id`, `project`, `request`, `investigated`, `learned`, `completed`, `next_steps`, `files_read`, `files_edited`, `notes`, `prompt_number`, `discovery_tokens`, `merged_into_project` (4 indexes) |
| `pending_messages` | Legacy durable work queue (now superseded by in-RAM `SessionMessageBuffer`; see §6) | `session_db_id`, `content_session_id`, `tool_use_id`, `message_type CHECK(observation/summarize)`, `status CHECK(pending/processing)`. UNIQUE index `ux_pending_session_tool` on `(content_session_id, tool_use_id)` for ingestion pairing |
| `user_prompts` | User prompt history | `content_session_id`, `prompt_number`, `prompt_text`, `created_at_epoch` (4 indexes) |
| `observation_feedback` | Usage signals feeding tier routing | `observation_id`, `signal_type`, `session_db_id` (2 indexes) |

### FTS5 virtual tables

- `user_prompts_fts` — created at `src/services/sqlite/SessionStore.ts:430`, tokenize=`porter-unicode61`, content=`user_prompts`, content_rowid=`id`. Triggers at `SessionStore.ts:455-468` (try/caught for builds without FTS5).
- `observations_fts` — `src/services/sqlite/SessionSearch.ts:75`. Columns: `title, subtitle, narrative, text, facts, concepts`. Triggers at `SessionSearch.ts:94-110`.
- `session_summaries_fts` — `SessionSearch.ts:113`. Columns: `request, investigated, learned, completed, next_steps, notes`.
- (server-beta variant) `memory_items_fts` at `src/storage/sqlite/schema.ts:171`, tokenize=`porter unicode61`, matching triggers at `:273-302`.

### Server-beta / Postgres schema (opt-in)

When `CLAUDE_MEM_RUNTIME=server-beta` (`src/services/hooks/runtime-selector.ts:33`), the worker routes through `ServerBetaClient` (`src/services/hooks/server-beta-client.ts`) hitting `/v1/*` against a Postgres backend (`src/storage/postgres/schema.ts`). Tables: `teams / projects / team_members / api_keys / audit_log / server_sessions / agent_events / observation_generation_jobs / observations / observation_sources / observation_generation_job_events`. The Postgres `observations.content_search` is a `TSVECTOR GENERATED ALWAYS AS (to_tsvector('english', content)) STORED` column with a GIN index (`schema.ts:219,289`).

---

## 3. Hook surface (the capture side)

### Claude Code hook registration — `plugin/hooks/hooks.json`

| Claude Code event | Matcher | Sub-event | Ref |
|---|---|---|---|
| `Setup` | `*` | `version-check.js` (no-op gate) | `:5-15` |
| `SessionStart` | `startup\|clear\|compact` | `worker-service start` (lifecycle) + `hook claude-code context` | `:18,24,30` |
| `UserPromptSubmit` | `*` | `hook claude-code session-init` | `:42` |
| `PreToolUse` | `Read` | `hook claude-code file-context` | `:68` |
| `PostToolUse` | `*` | `hook claude-code observation` | `:55` |
| `Stop` | `*` | `hook claude-code summarize` | `:80` |

Each handler is a shell command that calls `bun-runner.js` then `worker-service.cjs`. `plugin/scripts/bun-runner.js` collects the JSON payload from stdin (lines 96-146) and spawns `bun plugin/scripts/worker-service.cjs hook <platform> <event>`; on stdin failure it writes a `CAPTURE_BROKEN` marker file (lines 165-219, see §10).

Codex CLI gets the same shape but distinct event names (`plugin/hooks/codex-hooks.json:4,27,38,50,62`) dispatched as `hook codex <event>`. Cursor uses shell scripts under `cursor-hooks/hooks.json` (separate runtime).

### The hook command — `src/cli/hook-command.ts:85`

`hookCommand(platform, event)` is the single entry point:

1. Reads JSON payload from stdin via `src/cli/stdin-reader.ts:38` (30s safety timeout, falls back to `{}`).
2. Selects a **platform adapter** via `getPlatformAdapter()` (`src/cli/adapters/index.ts:9`) — Claude Code maps fields like `r.session_id → sessionId`, `r.tool_name → toolName`, etc. (`src/cli/adapters/claude-code.ts:9-26`).
3. Selects an **event handler** via `getEventHandler()` (`src/cli/handlers/index.ts:32`). Handlers are pure — no `process.exit`/`console.*`/`process.stderr.write` (comment at `src/cli/handlers/context.ts:1-7`).
4. Wraps everything in a stderr buffer (`src/shared/hook-io.ts`) so third-party logging doesn't leak into model context.
5. Emits `hookSpecificOutput.additionalContext` via stdout JSON, which Claude Code feeds back into the model as pre-prompt context.

### Event handlers → HTTP

Each handler is a thin wrapper over a single HTTP call (`executeWithWorkerFallback()` from `src/shared/worker-utils.ts`, with `AbortSignal.timeout(API_REQUEST_TIMEOUT_MS)` defaulting to 30s at `src/shared/hook-constants.ts:4`):

| Handler | HTTP call | Ref |
|---|---|---|
| `contextHandler` | `GET /api/context/inject?projects=...&colors=true\|false` | `src/cli/handlers/context.ts:19-87` |
| `sessionInitHandler` | `POST /api/sessions/init`; may also follow up with `POST /api/context/semantic?q=...` for semantic injection | `src/cli/handlers/session-init.ts:30,137` |
| `observationHandler` | `POST /api/sessions/observations` | `src/cli/handlers/observation.ts:42` |
| `summarizeHandler` | `POST /api/sessions/summarize` (extracts `last_assistant_message` from transcript via `src/shared/transcript-parser.ts`) | `src/cli/handlers/summarize.ts:17` |
| `fileContextHandler` | `GET /api/observations/by-file?path=...&projects=...&limit=40`. Skips injection when `fileMtimeMs >= newestObservationMs` | `src/cli/handlers/file-context.ts:139,248` |

---

## 4. Worker daemon (the brain)

### Process model

```
Claude Code (or Codex/Cursor/…)
   └─ node plugin/scripts/bun-runner.js plugin/scripts/worker-service.cjs <subcmd> [...]
        └─ (subcmd=hook) → CLI hook pipeline (adapter → handler → executeWithWorkerFallback)
   └─ (subcmd=start or first lazy-spawn) → bun plugin/scripts/worker-service.cjs <no-args>
        └─ WorkerService.start() → listens on 127.0.0.1:37777
             ├─ DatabaseManager ── SessionStore (SQLite)
             │                   └─ SessionSearch (FTS5)
             │                   └─ ChromaSync (chroma-mcp over stdio)
             ├─ SessionManager ── in-RAM SessionMessageBuffer per session
             ├─ ClaudeProvider / GeminiProvider / OpenRouterProvider
             │     └─ spawns claude-agent-sdk subprocesses (supervisor-tracked)
             ├─ SearchManager / SearchOrchestrator (semantic / hybrid / sqlite)
             ├─ Express HTTP server (routes — see table below)
             ├─ ChromaMcpManager (uvx chroma-mcp subprocess)
             └─ Supervisor (process-registry, signal handlers, graceful shutdown)
```

### Ports & env

- `CLAUDE_MEM_WORKER_PORT` (default `37777`) and `CLAUDE_MEM_WORKER_HOST` (default `127.0.0.1`) — `src/shared/SettingsDefaultsManager.ts` DEFAULTS.
- `CLAUDE_MEM_DATA_DIR` (default `~/.claude-mem`) — `paths.ts:18-40`.

### Subcommands — `src/services/worker-service.ts:772-799`

`start` / `stop` / `restart` / `status` / `hook <platform> <event>` / `server-start|stop|restart|status` / `server-logs|doctor|migrate|export|import` / `server-api-key create|list|revoke|migrate-scopes` / `cursor <sub>` / `gemini-cli <sub>` / `generate` / `clean` / `transcript <sub>` / `adopt` / `cleanup` / `--daemon` (the actual long-lived daemon).

### Lazy spawn flow — `src/services/worker-spawner.ts:71-178`

`ensureWorkerStarted()` is the orchestrator invoked from hooks and the MCP server:

1. `cleanStalePidFile()` (`src/services/infrastructure/ProcessManager.ts`) — if PID alive, wait for `/api/health`.
2. If port already answering health, return `'ready'`.
3. `acquireSpawnLock()` (`src/shared/worker-spawn-gate.ts`) — file-lock so multiple hooks don't race.
4. `spawnDaemon(workerScriptPath, port)` — detached process.
5. `waitForHealth(port, POST_SPAWN_WAIT=15s)` — polls `/api/health`.
6. `waitForReadiness(port, READINESS_WAIT=30s)` — polls `/api/readiness` (`hook-constants.ts:7`).

### Startup sequence — `src/services/worker-service.ts:375-417`

`WorkerService.start()`:

1. Enable telemetry exception autocapture + set logger error sink (`:385-386`).
2. Crash detection via `detectPreviousShutdown()` — reads stale PID + `.worker-clean-shutdown` sentinel (`:352-373, 136-176`).
3. `await startSupervisor()` — installs signal handlers (`src/supervisor/index.ts:35-47`).
4. `server.listen(port, host)`.
5. `writePidFile({pid, port, startedAt})`.
6. Supervisor `registerProcess('worker', ...)` (`:402`).
7. `initializeBackground()` (fire-and-forget) — runs heavy init: ModeManager, ChromaMcpManager lazy connect, `DatabaseManager.initialize()`, ChromaSync backfill (`:419-585`).

### HTTP routes

Registered in `worker-service.ts:294-340` (`registerRoutes`) plus `SearchRoutes` (`initializeBackground` at `:483`):

| Method | Path | Handler | Ref |
|---|---|---|---|
| GET | `/api/health` | Server | `Server.ts:212` |
| GET | `/api/readiness` | Server | `Server.ts:235` |
| GET | `/api/version` / `/api/instructions` | Server | `Server.ts:249,253` |
| POST | `/api/admin/restart` (localhost-only) | Server | `Server.ts:282` |
| POST | `/api/admin/shutdown` | Server | `Server.ts:296` |
| GET | `/api/admin/doctor` | Server | `Server.ts:317` |
| POST | `/api/sessions/init` | SessionRoutes | `SessionRoutes.ts:229` |
| POST | `/api/sessions/observations` | SessionRoutes | `SessionRoutes.ts:234` |
| POST | `/api/sessions/summarize` | SessionRoutes | `SessionRoutes.ts:239` |
| GET | `/api/sessions/status` | SessionRoutes | `SessionRoutes.ts:243` |
| GET | `/api/observations` | DataRoutes | `DataRoutes.ts:89` |
| GET | `/api/observations/by-file` | DataRoutes | `DataRoutes.ts:94` |
| GET | `/api/observation/:id` | DataRoutes | `DataRoutes.ts:93` |
| POST | `/api/observations/batch` | DataRoutes | `DataRoutes.ts:95` |
| GET | `/api/summaries` / `/api/prompts` / `/api/prompt/:id` | DataRoutes | `:90-91,98` |
| GET | `/api/session/:id` | DataRoutes | `:96` |
| POST | `/api/sdk-sessions/batch` | DataRoutes | `:97` |
| GET | `/api/stats` / `/api/projects` / `/api/processing-status` | DataRoutes | `:100-103` |
| POST | `/api/import` | DataRoutes | `:105` |
| GET | `/api/search` / `/api/timeline` / `/api/decisions` / `/api/changes` | SearchRoutes | `SearchRoutes.ts:138-141` |
| GET | `/api/search/observations\|sessions\|prompts\|by-concept\|by-file\|by-type` | SearchRoutes | `SearchRoutes.ts:144-149` |
| GET | `/api/context/recent\|timeline\|preview\|inject` | SearchRoutes | `SearchRoutes.ts:151-154` |
| POST | `/api/context/semantic` | SearchRoutes | `SearchRoutes.ts:155` |
| GET | `/api/onboarding/explainer` / `/api/timeline/by-query` / `/api/search/help` | SearchRoutes | `SearchRoutes.ts:156-159` |
| GET | `/api/chroma/status` | ChromaRoutes | registered `worker-service.ts:296` |
| GET | `/api/corpus`, `/api/corpus/:name/prime\|query\|rebuild\|reprime` | CorpusRoutes | `worker-service.ts:499` |
| POST | `/api/memory/save` | MemoryRoutes | `MemoryRoutes.ts:25` |

Plus BetterAuth + ServerV1 routes (auth + `/v1/*` server-beta endpoints). SSE feed for the viewer UI lives at `/api/events/stream` (`src/services/worker/SSEBroadcaster.ts:25-39`).

### Health/Readiness — `src/services/server/Server.ts:212-249`

`/api/health` returns `{status, version, workerPath, uptime, managed, hasIpc, platform, pid, initialized, mcpReady, ai, rateLimits, queue?}` — used by `restart-verify.ts` to confirm a restart succeeded (must show new pid + expected version, `src/services/restart-verify.ts:79-112`).

### Shutdown — `src/services/worker-shutdown.ts:64-176`

`runShutdownSequence()`:

1. Re-entrancy guard via `isShuttingDown()` (`:65-71`).
2. `beforeGracefulShutdown()` — stop transcript watcher, write clean-shutdown sentinel (`worker-service.ts:138-176`), emit `worker_stopped` telemetry (`:709-713`), call `shutdownTelemetry()`.
3. `performGracefulShutdown()` under hard deadline (10s default, Windows ×1.5 — `worker-service.ts:723`) — drains sessions, closes DB, stops ChromaMcp, closes MCP client.
4. **Only on restart**: successor handoff — `waitForPortFree` → `removePidFileIfOwner` → `spawnDaemon(successorScript, port)` (`:131-159`). CLI `restart` defers to this (`:1014-1033`).

The **self-replacing handoff** (dying worker spawns its successor before it exits) is the documented design — hook callers always wait on `/api/readiness` rather than spawning, so they don't race the corpse for the port (`worker-shutdown.ts:127-130`).

---

## 5. SDK compression pipeline (the LLM in the loop)

### Per-session generator — `src/services/worker/ClaudeProvider.ts:178-457`

For each session, `ClaudeProvider.startSession()` runs an SDK subprocess:

1. `query()` from `@anthropic-ai/claude-agent-sdk` is called with a message generator (`:232`). Subprocess spawned via `createSdkSpawnFactory` (`src/supervisor/process-registry.ts`); tracked by supervisor registry.
2. `for await (const message of queryResult)` consumes SDK events:
   - `system / rate_limit` → capture quota; abort if exceeded (`:255-278`).
   - `message.session_id` → persist via `ensureMemorySessionIdRegistered()`, store on `ActiveSession` (`:280-303`).
   - `message.type === 'assistant'` → capture usage, then call `processAgentResponse(text, session, …)` (`:379-390`).
3. **Output classifier** (`src/sdk/output-classifier.ts`) detects `idle` / `prose` / `poisoned` outputs; after 3 consecutive invalid outputs the SDK session is respawned (`ResponseProcessor.ts:25, 70-110` — "plan-11, #2485").

### Message generator — `ClaudeProvider.ts:459-554`

Yields one initial `init` prompt (built by `buildInitPrompt()` at `src/sdk/prompts.ts:24`), then for each pending message yields `buildObservationPrompt()` (`prompts.ts:117`, with 16k-char head/tail truncation) or `buildSummaryPrompt()` (`prompts.ts:153`).

### Parse & store — `src/services/worker/agents/ResponseProcessor.ts:27-280`

`processAgentResponse()`:

1. `parseAgentXml()` (`src/sdk/parser.ts:41`) expects `<observation>...</observation>` or `<summary>...</summary>` blocks.
2. Valid parse → `sessionStore.storeObservations(...)` (`SessionStore.ts:1901`, transactional with `ON CONFLICT(memory_session_id, content_hash) DO NOTHING`) and `sessionStore.storeSummary(...)` (`SessionStore.ts:1855`).
3. `contentHash = computeObservationContentHash(memorySessionId, title, narrative)` (`src/services/sqlite/observations/store.ts`).
4. **Chroma sync** — fire-and-forget `ChromaSync.syncObservation()` (`src/services/sync/ChromaSync.ts:301`) and `syncSummary()` (`:353`); see §7.
5. **SSE broadcast** — `broadcastObservation()` / `broadcastSummary()` (`src/services/worker/agents/ObservationBroadcaster.ts:6,28`) → `SSEBroadcaster` (`:6`) → live viewer UI at `plugin/ui/viewer-bundle.js`.

### Privacy check — `src/services/worker/validation/PrivacyCheckValidator.ts`

Gates which observations/summaries get stored based on user-prompt privacy detection. Specific rules aren't documented in the file header and weren't fully traced.

---

## 6. In-RAM queue vs durable pending_messages

This is the most non-obvious dataflow decision in the system:

- `SessionStore.ts:500-538` still creates and migrates `pending_messages`, and `schema.sql:124-150` defines it with the `ux_pending_session_tool` UNIQUE index.
- The **running** code path uses `SessionMessageBuffer` (`src/services/worker/SessionMessageBuffer.ts:42`) — an in-RAM buffer — per the doc comment at `:22-41`. The transcript JSONL is the real durable source; `pending_messages` appears to be a legacy recovery path or dead code.

Dedup is by `toolUseId` against an in-memory `seenToolUseIds` set (`SessionMessageBuffer.ts:56-71`).

---

## 7. Chroma / vector integration

### Startup — `src/services/sync/ChromaMcpManager.ts`

Singleton spawned from `DatabaseManager` via `ChromaSync`:

- **Default mode** `CLAUDE_MEM_CHROMA_MODE=local` → spawns `uvx --python 3.13 --with onnxruntime>=1.20 --with protobuf<7 chroma-mcp==0.2.6 --client-type persistent --data-dir ~/.claude-mem/chroma` as a stdio MCP subprocess (`ChromaMcpManager.ts:241-248`). Pin rationale (`onnxruntime>=1.20`, `protobuf<7`) at `:30-44`.
- **Remote mode** switches to `--client-type http --host --port` (`:207-238`).
- **Singleton enforcement** (`#2313`): tree-kills prior subprocess before reconnect (`:396-425`); uses `pgrep -P` walks + `taskkill /T /F` on Windows (`:465-579`).
- The spawned child is registered with the supervisor (PID + pgid, `:783-813`).

`DatabaseManager.initialize()` instantiates `ChromaSync('claude-mem')` (`DatabaseManager.ts:23-29`). `ChromaSync` builds a per-project collection named `cm__<sanitized-project>` (`ChromaSync.ts:64-70`), e.g. `cm__thedotmack_claude-mem`.

### Document shape — `src/services/sync/ChromaSync.ts:97-218`

Each observation is split into 1-3 documents:

- `obs_<id>_narrative` (if narrative set)
- `obs_<id>_text` (legacy, if text set)
- `obs_<id>_fact_<index>` (one per fact)

Summaries split into `summary_<id>_request`, `_investigated`, `_learned`, `_completed`, `_next_steps`, `_notes`. User prompts: `prompt_<id>` (`:398-411`).

Metadata includes `sqlite_id`, `doc_type`, `memory_session_id`, `project`, `merged_into_project`, `created_at_epoch`, plus `type` / `title` / `concepts` / `files_read` / `files_modified` for observations.

### Add path — `ChromaSync.ts:229-299`

`addDocuments()` batches by `BATCH_SIZE=100` and calls `chroma_add_documents` over MCP. On "already exists" it retries with `chroma_delete_documents` + `chroma_add_documents` (reconcile). Watermark advances only when **all docs in the observation landed** (`:340-350, 385-395, 438-448`). Watermark bookkeeping lives in `~/.claude-mem/chroma-sync-state.json` (`src/services/sync/ChromaSyncState.ts`).

### Query path — `src/services/worker/search/SearchOrchestrator.ts:24-160`

Routes to one of three strategies:

- `ChromaSearchStrategy` — free-text → semantic.
- `HybridSearchStrategy` — `findByConcept` / `findByType` / `findByFile`: combines Chroma semantic match with a 90-day recency filter, then hydrates SQLite rows via `getObservationsByIds` (`src/services/worker/SearchManager.ts:100-124`).
- `SQLiteSearchStrategy` — fallback when Chroma is disabled or unavailable.

`SearchManager.buildDocTypeWhereFilter()` (`SearchManager.ts:80-92`) builds `{$and: [doc_type filter, $or: [{project}, {merged_into_project}]]}` — every Chroma query is scoped to the requested project plus any merged-in projects.

### Bootstrapping — `ChromaSync.backfillAllProjects()` (`ChromaSync.ts:940-1027`)

Runs at worker boot (`worker-service.ts:567-572`), three projects at a time (`BACKFILL_CONCURRENCY_LIMIT = 3`, `ChromaSync.ts:923`). Bootstraps watermarks from existing Chroma collections on first run (`bootstrapWatermarksFromChroma`, `:518-530`), then incremental backfills from each project's SQLite.

### Disabled mode

If `CLAUDE_MEM_CHROMA_ENABLED=false`, `ChromaMcpManager` is never constructed and `ChromaSync` is null (`worker-service.ts:460-466`, `DatabaseManager.ts:24-29`). All searches fall back to SQLite FTS5 (`SessionSearch.ts:268-296`). SearchManager logs `search_strategy: 'sqlite'` and `chroma_available: false` into the `search_performed` telemetry envelope (`SearchManager.ts:28-33`).

---

## 8. Recall pipeline (session-start → context injection)

### SessionStart hook → `GET /api/context/inject`

`contextHandler` (`src/cli/handlers/context.ts:19-87`) calls `GET /api/context/inject?projects=<csv>&colors=true|false` on the worker. Route handler is `SearchRoutes.handleContextInject` (`src/services/worker/http/routes/SearchRoutes.ts:378-460`):

1. If `CLAUDE_MEM_WELCOME_HINT_ENABLED=true` AND no observations exist for any of the requested projects (cached in `projectsKnownNonEmpty`, `SearchRoutes.ts:90-103,400-413`), it returns a one-time "welcome" markdown pointing the user at `http://localhost:<port>` (the viewer).
2. Otherwise imports `generateContextWithStats` (`src/services/context-generator.ts:3` → `src/services/context/ContextBuilder.ts:162`).

### `generateContextWithStats` → `ContextBuilder.buildContextOutput` — `ContextBuilder.ts:162-209`

1. `loadContextConfig()` (`src/services/context/ContextConfigLoader.ts:7`) — pulls observation limits, observation-type/concept filters from `~/.claude-mem/settings.json` via `SettingsDefaultsManager`.
2. `queryObservations()` / `querySummaries()` (`src/services/context/ObservationCompiler.ts:18,63`) — JOIN against `sdk_sessions` to surface `platform_source`, scoped to project, filtered by mode (active mode loaded via `ModeManager`, controlled by `CLAUDE_MEM_MODE`).
3. `buildTimeline()` interleaves observations and summaries chronologically.
4. `renderHeader` → `renderTimeline` → `renderSummaryFields` → `renderPreviouslySection` → `renderFooter` (all in `src/services/context/sections/`).
5. `calculateTokenEconomics()` (`src/services/context/TokenCalculator.ts`) — figures the savings vs. naive ("would have read N tokens instead of injecting M tokens").

Rendered markdown string is returned as `text/plain`, goes back through the hook's stdout JSON in `hookSpecificOutput.additionalContext`. Claude Code then injects it into the model as pre-prompt context. If `showTerminalOutput` is on, a colored variant is also fetched and surfaced as a `systemMessage`.

### Other recall entry points

- `GET /api/observations/by-file` → `DataRoutes.handleGetObservationsByFile` (`DataRoutes.ts:141-163`) — powers the PreToolUse `file-context` hook. Accepts repeated `path` query params (absolute and cwd-relative forms, de-duped) so PostToolUse and PreToolUse paths can match (`:147-152`).
- `POST /api/context/semantic` → `SearchRoutes.handleSemanticContext` (`SearchRoutes.ts:462-499`) — calls `searchManager.search({query, type: 'observations', format: 'json'})`; powers `session-init`'s semantic injection path (`session-init.ts:131-146`).
- `GET /api/search/observations` — full-text via `SessionSearch.searchObservations` (`SessionSearch.ts:244`), using FTS5 ranking when available else LIKE fallback.
- `GET /api/search` (unified) → `SearchManager.search` → `SearchOrchestrator` (`src/services/worker/search/SearchOrchestrator.ts:24`).
- The MCP server's `search` / `timeline` / `get_observations` tools (`src/servers/mcp-server.ts:465,488,507`) proxy these same HTTP routes via `callWorkerAPI()` (`:75-114`).

---

## 9. MCP / OpenCode / Cursor / Gemini / Windsurf integrations

### Claude Code MCP — `plugin/.mcp.json`

Single stdio MCP server `mcp-search` spawning `node mcp-server.cjs`. The tool surface is `src/servers/mcp-server.ts` (~1054 lines):

- `search` (`:465`) → `GET /api/search`.
- `timeline` (`:488`) → `GET /api/timeline`.
- `get_observations` (`:507`) → `POST /api/observations/batch`.
- `observation_add` / `observation_record_event` / `observation_search` / `observation_context` / `observation_generation_status` (server-beta REST, `:531-610`).
- `memory_add` / `memory_search` / `memory_context` — compatibility aliases for `observation_*` (`:616-675`).
- `smart_search` / `smart_unfold` / `smart_outline` — tree-sitter AST-based code reading (`:677-787`, `src/services/smart-file-read/`).
- `build_corpus` / `list_corpora` / `prime_corpus` / `query_corpus` / `rebuild_corpus` / `reprime_corpus` — knowledge corpus tools (`:789-892`), backed by `/api/corpus*` (`src/services/worker/http/routes/CorpusRoutes.ts`).

The MCP server self-checks its marketplace marker on boot (`:995-1019`), runs parent-heartbeat supervision (`:965-980`), and only auto-starts the worker when `selectRuntime()` is `'worker'` (`:1036-1048`).

### OpenCode plugin — `src/integrations/opencode-plugin/index.ts` (~336 lines)

`ClaudeMemPlugin` returns an object with hook handlers:

- `tool.execute.after` (`:193-205`) → `POST /api/sessions/observations` with `tool_name=input.tool, tool_input=output.args, tool_response=truncate(output.output, 1000)`.
- `chat.message` (`:208-230`) → posts assistant messages as observations with `tool_name="assistant_message"`.
- `experimental.session.compacting` (`:234-242`) → `POST /api/sessions/summarize`.
- `event` bus (`:246-270`) → reacts only to real bus types (`session.idle`, `session.deleted`) — `REAL_OPENCODE_EVENT_TYPES` at `:30-33`.
- `tool.claude_mem_search` (`:272-296`) → `GET /api/search/observations?query=...&limit=10`.

Session id mapping is local in-memory: `getOrCreateContentSessionId()` derives `opencode-<sessionID>-<timestamp>` (`:142-159`). `ensureSessionInitialized()` lazily initializes the worker session (`:166-177`).

### Cursor

Shell scripts under `cursor-hooks/` registered via `cursor-hooks/hooks.json` (separate protocol; not investigated in depth). The worker integration happens through `updateCursorContextForProject` (`src/services/integrations/CursorHooksInstaller.ts`, called from `ResponseProcessor.ts:449` after every summary).

### Gemini CLI

Adapter at `src/cli/adapters/gemini-cli.ts`, installer at `src/services/integrations/GeminiCliHooksInstaller.ts`. Hook scripts bundled in the marketplace directory.

### Windsurf

Adapter at `src/cli/adapters/windsurf.ts`, installer at `src/services/integrations/WindsurfHooksInstaller.ts`.

---

## 10. End-to-end trace of one observation

User runs a Claude Code session. Tool `Read` fires on `/path/to/file.ts`.

1. **`plugin/hooks/hooks.json:55`** matches `PostToolUse`. Shell: `node plugin/scripts/bun-runner.js plugin/scripts/worker-service.cjs hook claude-code observation`.
2. **`plugin/scripts/bun-runner.js`** collects stdin JSON (`payload = {session_id, cwd, tool_name: "Read", tool_input: {file_path: "/path/to/file.ts"}, tool_response: "..."}`), spawns `bun plugin/scripts/worker-service.cjs hook claude-code observation` (lines 96-146). On stdin failure, writes `CAPTURE_BROKEN` marker (lines 165-219 — see §11 #7).
3. **`worker-service.cjs`** runs `main()` → `case 'hook'` (`worker-service.ts:1212-1235`). Calls `ensureWorkerStarted(port)` (`worker-spawner.ts:71`), then `hookCommand('claude-code', 'observation')`.
4. **`hook-command.ts:85`** selects `claudeCodeAdapter.normalizeInput()` (`claude-code.ts:9-26`) and `observationHandler` (`handlers/index.ts:32`).
5. **`observationHandler.execute()`** (`observation.ts:42-98`) checks project, then calls `executeWithWorkerFallback('/api/sessions/observations', 'POST', {contentSessionId, platformSource, tool_name, tool_input, tool_response, cwd, agentId, agentType})`.
6. **`SessionRoutes.handleObservationsByClaudeId`** (`SessionRoutes.ts:274-311`) → `ingestObservation()` (`http/shared.ts:58-143`). Resolves `contentSessionId → sessionDbId` via `store.createSDKSession()` (`SessionStore.ts:1692`), privacy-checks (`PrivacyCheckValidator`), strips memory tags, enqueues onto `SessionMessageBuffer` (`SessionMessageBuffer.ts:56-71`), calls `ensureGeneratorRunning(sessionDbId, 'observation')` (`SessionRoutes.ts:90-112`).
7. **Generator** (`ClaudeProvider.startSession()` `ClaudeProvider.ts:178-457`): drains buffered message via `SessionManager.getMessageIterator` (`SessionManager.ts:404-434`), yields `buildObservationPrompt(tool, input, output)` (`sdk/prompts.ts:117`) into the SDK `query()`. SDK subprocess returns `<observation>...<type>discovery</type><title>Read /path/to/file.ts</title>...</observation>` (or empty / skipped).
8. **`ResponseProcessor.processAgentResponse`** (`agents/ResponseProcessor.ts:27-280`): `parseAgentXml()` (`sdk/parser.ts:41`) → valid → `sessionStore.storeObservations(memorySessionId, project, labeledObservations, summaryForStore, lastPromptNumber, discoveryTokens, originalTimestamp, modelId)` (`SessionStore.ts:1901`, transactional with `ON CONFLICT(memory_session_id, content_hash) DO NOTHING`).
9. **Fire-and-forget ChromaSync** (`ResponseProcessor.ts:329-352`): `ChromaSync.syncObservation(obsId, …)` formats into 1-3 documents (`ChromaSync.ts:97-154`), batches by 100, calls `chroma_add_documents` (`ChromaSync.ts:249`), bumps `chroma-sync-state.json` watermark on success (`ChromaSync.ts:342-350`).
10. **SSE broadcast** (`ResponseProcessor.ts:354-371`) via `broadcastObservation()` → live viewer UI sees the new row.
11. Hook returns `{continue: true, suppressOutput: true}`. Hook exits 0.

**Next session**, when the user opens Claude Code again:

12. **`plugin/hooks/hooks.json:18-30`** fires `SessionStart` (startup|clear|compact). Two commands run: `worker-service.cjs start` (lazy-spawn gate) and `worker-service.cjs hook claude-code context`.
13. **`contextHandler.execute()`** (`handlers/context.ts:19-87`) calls `executeWithWorkerFallback('/api/context/inject?projects=<csv>&colors=true|false', 'GET')`.
14. **`SearchRoutes.handleContextInject`** (`SearchRoutes.ts:378-460`) → `generateContextWithStats({session_id, cwd, projects, full})` (`context/ContextBuilder.ts:162-209`).
15. **Render**: `queryObservations` (`ObservationCompiler.ts:18`), `querySummaries` (`ObservationCompiler.ts:63`), `buildTimeline`, then `renderHeader` → `renderTimeline` → `renderSummaryFields` → `renderPreviouslySection` → `renderFooter` produces a markdown string with the discovery from step 11.
16. Hook emits `{hookSpecificOutput: {hookEventName: 'SessionStart', additionalContext: '<the markdown>'}}` via stdout JSON → Claude Code injects it as the model's pre-prompt context.

---

## 11. Configuration knobs (the meaningful ones)

Defaults from `src/shared/SettingsDefaultsManager.ts`:

- `CLAUDE_MEM_MODEL` (default `claude-sonnet-4-5`) — overrides via `$TIER:fast|smart|simple|summary` aliases (`src/services/worker/model-aliases.ts`).
- `CLAUDE_MEM_CONTEXT_OBSERVATIONS` (50), `CLAUDE_MEM_CONTEXT_SESSION_COUNT` (10), `CLAUDE_MEM_CONTEXT_FULL_COUNT` (5), `CLAUDE_MEM_CONTEXT_FULL_FIELD` (`narrative`).
- `CLAUDE_MEM_MODE` (`code`) — selects a mode JSON under `~/.claude-mem/modes/` or `plugin/modes/` (`code.json` defines observation types `bugfix,feature,refactor,discovery,decision,change` and concepts `how-it-works,why-it-exists,what-changed,…`). Mode-driven prompt templates loaded by `ModeManager` (`src/services/domain/ModeManager.ts`).
- `CLAUDE_MEM_SKIP_TOOLS` (`ListMcpResourcesTool,SlashCommand,Skill,TodoWrite,AskUserQuestion`) — applied at `src/services/worker/http/shared.ts:71-76`.
- `CLAUDE_MEM_EXCLUDED_PROJECTS`, `CLAUDE_MEM_CHROMA_ENABLED`, `CLAUDE_MEM_CHROMA_MODE` (`local|remote`), `CLAUDE_MEM_PYTHON_VERSION` (`3.13`).
- `CLAUDE_MEM_RUNTIME` (`worker` | `server-beta`) — gates the Postgres fallback (`src/services/hooks/runtime-selector.ts:33`).
- `CLAUDE_MEM_MAX_CONCURRENT_AGENTS` (2) — SDK concurrency cap (`ClaudeProvider.ts:204`).
- `CLAUDE_MEM_TIER_ROUTING_ENABLED` + `CLAUDE_MEM_TIER_SUMMARY_MODEL` / `_SIMPLE_MODEL` — tier routing at `SessionRoutes.ts:554-594`.
- `CLAUDE_MEM_FOLDER_CLAUDEMD_ENABLED` — updates `.claude.md` files in modified dirs after compression (`ResponseProcessor.ts:374-395`).
- `CLAUDE_MEM_SEMANTIC_INJECT` + `CLAUDE_MEM_SEMANTIC_INJECT_LIMIT` (5) — extra semantic search at `session-init` (`session-init.ts:131-146`).
- `CLAUDE_MEM_TRANSCRIPTS_ENABLED` + `CLAUDE_MEM_TRANSCRIPTS_CONFIG_PATH` — gate the standalone transcript watcher (`worker-service.ts:619-679`).
- `CLAUDE_MEM_CODEX_TRANSCRIPT_INGESTION` — opt-in for Codex JSONL ingestion when native hooks exist (`worker-service.ts:636-648`).
- `CLAUDE_MEM_WELCOME_HINT_ENABLED` — gates the one-time welcome markdown at SessionStart (`SearchRoutes.ts:90-103,400-413`).

### Env / auth isolation

`src/shared/EnvManager.ts` (`buildIsolatedEnvWithFreshOAuth()`) provides the worker's isolated env. `src/supervisor/env-sanitizer.ts` strips host-CLI bleed-through (`CLAUDE_CODE_*` env vars, including `EFFORT_LEVEL` — see `ClaudeProvider.ts:107-138` for the "effort parameter" 400-class bug).

---

## 12. Open questions / unclear bits

1. **In-RAM queue vs durable `pending_messages`** — `SessionStore.ts:500-538` still creates and migrates `pending_messages`; `schema.sql:124-150` defines it with `ux_pending_session_tool` UNIQUE. But the running code path uses `SessionMessageBuffer` (`src/services/worker/SessionMessageBuffer.ts:42`) — the doc comment says the transcript JSONL is the real durable source. Status of the table: legacy / dead code vs fallback?
2. **`agent_events` table in worker SQLite** — `src/storage/sqlite/schema.ts` defines an `agent_events` table (different from the Postgres one) but no obvious insert path. No `/api/agent-events` route. Likely server-beta-only.
3. **`memory_sources` table** — defined at `src/storage/sqlite/schema.ts:107-117`; no traced insert path.
4. **Server-beta runtime gap** — when `CLAUDE_MEM_RUNTIME=server-beta`, MCP server doesn't auto-spawn the worker (`mcp-server.ts:1036-1048`); hooks route through `ServerBetaClient` (`src/services/hooks/server-beta-client.ts`, ~13.4KB) hitting `/v1/*` REST. The Postgres backend (`src/storage/postgres/schema.ts`) has its own job-queue table (`observation_generation_jobs` with BullMQ `bullmq_job_id`). Not fully traced.
5. **Chroma MCP tool names** — `ChromaMcpManager.callTool()` calls `chroma_create_collection`, `chroma_add_documents`, `chroma_delete_documents`, `chroma_get_documents`, `chroma_query_documents`, `chroma_list_collections`, `chroma_update_documents`. Not verified against the actual `chroma-mcp==0.2.6` exposed tool names.
6. **MCP server tool surface vs `opencode-plugin`** — OpenCode's `tool.claude_mem_search` only exposes search (`index.ts:272-296`). The full MCP tool surface (`search`, `timeline`, `get_observations`, plus corpus/smart_*) is available only via Claude Code's MCP wiring — OpenCode users get a thinner integration.
7. **Hook payload diagnostics** — `plugin/scripts/bun-runner.js:165-219` writes a `CAPTURE_BROKEN` marker file when stdin is empty/missing; a recent workaround (#2188). Implies the hook pipeline has had stdin-reliability issues across platforms.
8. **Privacy checker rules** — `src/services/worker/validation/PrivacyCheckValidator.ts` was only skimmed. It gates which observations/summaries get stored based on user-prompt privacy detection; exact rules aren't documented inline.

---

## 13. Strengths & weaknesses (realistic take)

### Strengths

- **Single source of truth** — `src/services/sqlite/schema.sql` with the comment "Authoritative shape" plus matching inline migrations in `SessionStore` constructor. Schema version is explicit (`SERVER_STORAGE_SCHEMA_VERSION = 33`). No mystery state machines.
- **Dedup at the storage layer** — `UNIQUE(memory_session_id, content_hash)` + `INSERT … ON CONFLICT DO NOTHING` (`schema.sql:80`, `SessionStore.ts:1901`) is the right place for dedup. The hash function `computeObservationContentHash(memorySessionId, title, narrative)` makes the key deterministic.
- **Process isolation done right** — worker is detached + supervisor-tracked; restart is a self-replacing handoff (`worker-shutdown.ts:131-159`); hook callers wait on `/api/readiness` rather than spawning. Multiple hooks (e.g. MCP + PostToolUse) can't race for the port because `acquireSpawnLock()` (`worker-spawn-gate.ts`) serializes them.
- **Pluggable search** — three strategies (Chroma / hybrid / SQLite FTS5) behind one orchestrator (`SearchOrchestrator.ts:24`). Chroma can be disabled at runtime and the system degrades gracefully.
- **Project scoping** — `merged_into_project` lets you fold one project's memory into another; both `SearchManager.buildDocTypeWhereFilter()` and `ChromaSync` honor it.
- **Mode system** — `CLAUDE_MEM_MODE=code` (or others) drives the prompt templates and observation types. Clean separation of "what kind of project is this" from "how do we render memory".

### Weaknesses / sharp edges

- **In-RAM queue loses durability** — if the worker crashes between `ingestObservation` and `processAgentResponse`, the message is gone (the durable `pending_messages` table is bypassed). The transcript JSONL is meant to be the recovery source, but only if `CLAUDE_MEM_TRANSCRIPTS_ENABLED=true`. Otherwise: silent data loss on crash.
- **STDIN reliability** — `bun-runner.js` having to write `CAPTURE_BROKEN` markers (issue #2188) shows the hook entry point has been flaky across platforms. Wrapping with a retry-on-empty-stdin would be cleaner than a marker file.
- **`pending_messages` ambiguity** — schema comment says it's the "current work queue"; code comment in `SessionMessageBuffer` says it's legacy. Pick one and remove the other; right now a future reader has to trace both.
- **Chroma MCP version pin fragility** — `onnxruntime>=1.20` + `protobuf<7` pins (`ChromaMcpManager.ts:30-44`) suggest the team has been bitten by transitive dep breakage. Hardcoded `chroma-mcp==0.2.6` means upstream breakage hits silently.
- **Server-beta incomplete** — `agent_events` / `memory_sources` in `src/storage/sqlite/schema.ts` have no traced insert path. This is technical debt that will confuse new contributors.
- **No documented test for the full pipeline** — `tests/` exists (27 entries in the project root) but the E2E path of "hook fires → SDK subprocess → SQLite row → Chroma doc → SessionStart context" isn't pinned by a single integration test in the files I traced.
- **OpenCode integration is second-class** — `tool.claude_mem_search` is the only memory tool the OpenCode plugin exposes (`index.ts:272-296`). Compared to the full MCP server tool list, that's ~1/10th the surface.
- **Privacy rules undocumented** — `PrivacyCheckValidator` is a gate, but the rules aren't visible in the file I read. For a system that captures "everything", the privacy contract should be in the repo README or a dedicated doc.
- **Single-port design** — everything goes through `127.0.0.1:37777`. If that port is taken by something else (or you're running multiple workers for different projects), you have to override `CLAUDE_MEM_WORKER_PORT` per project and the marketplace marker detection (`plugin/.mcp.json`) gets complicated.
- **Failure isolation between Chroma and SQLite** — when ChromaSync is fire-and-forget (`ResponseProcessor.ts:329-352`), failures only show up in the watermark file (`chroma-sync-state.json`). A user inspecting the SQLite DB won't realize their semantic search is silently stale.

---

## 14. TL;DR dataflow diagram

```
┌─────────────────────────────────────────────────────────────────────────────┐
│  Claude Code session                                                         │
│                                                                              │
│   SessionStart ──► hooks.json:18 ──► worker-service.cjs hook context         │
│                                       │                                      │
│   UserPrompt ───► hooks.json:42 ──► hook session-init ──► /api/sessions/init │
│                                       │                                      │
│   PreToolUse:Read ► hooks.json:68 ──► hook file-context ──► /api/obs/by-file │
│                                       │                                      │
│   PostToolUse ──► hooks.json:55 ──► hook observation ──► /api/sessions/obs   │
│                                       │                                      │
│   Stop ─────────► hooks.json:80 ──► hook summarize ──► /api/sessions/summarize│
└─────────────────────────────────────────────────────────────────────────────┘
                                       │
                                       ▼  HTTP (127.0.0.1:37777)
┌─────────────────────────────────────────────────────────────────────────────┐
│  Worker daemon (Node/Bun, supervisor-tracked)                                │
│                                                                              │
│   ┌── Routes ──┐                                                             │
│   │ SessionRoutes │                                                          │
│   │ SearchRoutes  │◄── /api/context/inject (recall)                          │
│   │ DataRoutes    │◄── /api/observations/by-file                             │
│   │ ChromaRoutes  │◄── /api/chroma/status                                    │
│   │ MemoryRoutes  │◄── /api/memory/save                                      │
│   └──────────────┘                                                           │
│        │                                                                      │
│        ▼                                                                      │
│   ┌── Ingestion ────────────────────────────────────────────────────────┐    │
│   │  PrivacyCheckValidator → SessionMessageBuffer (in-RAM, per session) │   │
│   │  ensureGeneratorRunning() → ClaudeProvider.startSession()             │   │
│   └──────────────────────────────────────────────────────────────────────┘   │
│        │                                                                      │
│        ▼                                                                      │
│   ┌── Compression (Claude Agent SDK subprocess) ─────────────────────────┐   │
│   │  buildInitPrompt / buildObservationPrompt / buildSummaryPrompt         │   │
│   │  → SDK query() → ResponseProcessor.processAgentResponse()              │   │
│   │  → parseAgentXml() → storeObservations() (transactional)               │   │
│   └──────────────────────────────────────────────────────────────────────┘   │
│        │                                                                      │
│        ▼                                                                      │
│   ┌── Persistence ───────────────┐    ┌── Vector mirror ─────────────────┐   │
│   │  SQLite @ ~/.claude-mem/…db  │    │  Chroma MCP @ ~/.claude-mem/chroma│   │
│   │  UNIQUE(mem_session, hash)   │    │  Per-project collection cm__<p>   │   │
│   │  FTS5: observations_fts, …   │    │  docs: obs_<id>_{narrative|text|fact_i}│  │
│   └──────────────────────────────┘    └──────────────────────────────────┘   │
│                                                                              │
│   SSEBroadcaster ──► /api/events/stream ──► plugin/ui/viewer-bundle.js       │
└─────────────────────────────────────────────────────────────────────────────┘
                                       │
                                       ▼  recall (SessionStart → context)
┌─────────────────────────────────────────────────────────────────────────────┐
│  Next Claude Code session                                                    │
│                                                                              │
│   /api/context/inject ──► ContextBuilder.buildContextOutput                 │
│        │                                                                      │
│        ├── queryObservations() / querySummaries() (JOIN sdk_sessions)        │
│        ├── buildTimeline()                                                   │
│        ├── renderHeader / Timeline / SummaryFields / Previously / Footer    │
│        └── calculateTokenEconomics()                                         │
│                                                                              │
│   → hookSpecificOutput.additionalContext (stdout JSON)                       │
│   → Claude Code injects as model pre-prompt context                          │
└─────────────────────────────────────────────────────────────────────────────┘
```

---

## 15. File reference cheat-sheet

| Layer | Key files |
|---|---|
| Hook registration | `plugin/hooks/hooks.json`, `plugin/hooks/codex-hooks.json`, `cursor-hooks/hooks.json`, `plugin/.mcp.json` |
| Hook entry | `plugin/scripts/bun-runner.js`, `plugin/scripts/worker-service.cjs`, `src/services/worker-service.ts:1212-1235` |
| CLI dispatch | `src/cli/hook-command.ts`, `src/cli/handlers/{context,session-init,observation,summarize,file-context}.ts`, `src/cli/adapters/{claude-code,gemini-cli,windsurf}.ts`, `src/cli/stdin-reader.ts`, `src/shared/hook-io.ts` |
| Worker HTTP routes | `src/services/server/Server.ts`, `src/services/worker/http/routes/{SessionRoutes,DataRoutes,SearchRoutes,ChromaRoutes,CorpusRoutes,MemoryRoutes}.ts` |
| Worker service core | `src/services/worker-service.ts`, `src/services/worker-spawner.ts`, `src/services/worker-shutdown.ts`, `src/services/restart-verify.ts` |
| SDK compression | `src/services/worker/ClaudeProvider.ts`, `src/services/worker/agents/ResponseProcessor.ts`, `src/services/worker/agents/ObservationBroadcaster.ts`, `src/services/worker/SessionManager.ts`, `src/services/worker/SessionMessageBuffer.ts`, `src/services/worker/validation/PrivacyCheckValidator.ts`, `src/sdk/parser.ts`, `src/sdk/prompts.ts`, `src/sdk/output-classifier.ts` |
| Storage | `src/services/sqlite/schema.sql`, `src/services/sqlite/SessionStore.ts`, `src/services/sqlite/SessionSearch.ts`, `src/storage/sqlite/schema.ts`, `src/storage/postgres/schema.ts` |
| Vector | `src/services/sync/ChromaMcpManager.ts`, `src/services/sync/ChromaSync.ts`, `src/services/sync/ChromaSyncState.ts` |
| Search | `src/services/worker/search/{SearchOrchestrator,SearchManager}.ts`, `src/services/worker/http/routes/SearchRoutes.ts` |
| Context rendering | `src/services/context/ContextBuilder.ts`, `src/services/context/ObservationCompiler.ts`, `src/services/context/ContextConfigLoader.ts`, `src/services/context/TokenCalculator.ts`, `src/services/context/sections/*` |
| MCP | `src/servers/mcp-server.ts`, `plugin/scripts/mcp-server.cjs` |
| Integrations | `src/integrations/opencode-plugin/index.ts`, `src/cli/adapters/gemini-cli.ts`, `src/services/integrations/{CursorHooksInstaller,GeminiCliHooksInstaller,WindsurfHooksInstaller}.ts` |
| Supervisor | `src/supervisor/index.ts`, `src/supervisor/process-registry.ts`, `src/supervisor/env-sanitizer.ts`, `src/services/infrastructure/ProcessManager.ts` |
| Domain modes | `src/services/domain/ModeManager.ts`, `src/shared/SettingsDefaultsManager.ts`, `plugin/modes/*.json`, `~/.claude-mem/modes/*.json` |
| Paths & settings | `src/shared/paths.ts`, `src/shared/EnvManager.ts`, `src/shared/hook-constants.ts` |
| SSE / UI | `src/services/worker/SSEBroadcaster.ts`, `plugin/ui/viewer-bundle.js` |