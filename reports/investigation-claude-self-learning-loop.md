# Investigation: `claude-self-learning-loop`

**Source repo:** `/home/blucky/Agents/ref/claude-self-learning-loop/`
**Investigated:** server, hooks, skill, config, Slack gate
**Files inspected (with line counts):** README.md:110, server.py:269, pytest_tracker.py:123, stop_lesson_check.py:106, search_hook.py:89, SKILL.md:142, settings.json.example:31, slack/hook.py:120

This is a "Claude Code that learns from its pytest failures" kit. The whole system is <800 lines of Python plus one Skill prompt. The interesting idea is **not** the vector DB — it's the *stop hook as a memory commit gate*. The vector DB and Ollama are commodity scaffolding; the design choice that does the actual work is forcing Claude to distill an RCA before it is allowed to stop.

---

## 1. Architecture: Save Path + Retrieve Path

The repo's own README diagram (`README.md:10-28`) is accurate. Two paths share one ChromaDB collection (`lessons`, cosine space, server.py:36-39).

### 1.1 Save path (pytest fail → fix → lesson saved)

```
pytest fails
   │  PostToolUse(Bash) hook fires
   ▼
pytest_tracker.py  →  /tmp/claude_pending_lessons_{session_id}.json  (status: pending)
   │  ... Claude fixes ...
   ▼
pytest_tracker.py  →  same state file, status flips to "resolved"
   │  Claude calls Stop
   ▼
stop_lesson_check.py  →  decision: block + systemMessage listing unresolved errors
   │  Claude must invoke
   ▼
Skill: lesson-recorder (5 Whys + 3-sentence distillation, noise filter)
   │  invokes
   ▼
mcp__memory__save_lesson  (server.py:148-188)
   │  embeds via Ollama nomic-embed-text (server.py:43-54)
   ▼
ChromaDB persistent collection at <repo>/claude-memory-mcp/data/lessons
```

Concrete entry points:

- **Tracker write** — `claude-memory-mcp/pytest_tracker.py:96-106` appends an entry with `error_signature`, `error_type`, `failed_tests[:5]`, `summary_lines[:5]`, `test_command[:200]`. Signature dedup uses sorted top-5 failing tests (`pytest_tracker.py:45-47`).
- **Tracker resolve** — `pytest_tracker.py:108-117` only flips pending → resolved when both `RE_PASSED_ALL` matches **and** `RE_STILL_FAILED` does **not** (`pytest_tracker.py:23-24`). That's important: `pytest -k foo` outputs both "X passed" and "Y failed" — the second regex prevents false-positive resolutions.
- **Stop blocker** — `claude-memory-mcp/stop_lesson_check.py:74-101` builds `error_blocks` from every resolved error and returns `{"decision":"block", "systemMessage": ...}`.
- **MCP save** — `claude-memory-mcp/server.py:148-188`. Embedding text is `f"Error: {error_summary} Cause: {root_cause} Fix: {solution}"` (`server.py:156`) — the three distilled fields are concatenated and embedded as one vector. Doc id is `lesson_{YYYYMMDD_HHMMSS_microseconds}` (`server.py:160`) — microseconds avoid collision when saving multiple in one second.

### 1.2 Retrieve path (user prompt → relevant lessons injected)

```
UserPromptSubmit
   │
   ▼
search_hook.py  (claude-memory-mcp/search_hook.py:33-85)
   │  embeds the raw prompt via Ollama
   │  queries ChromaDB directly (no MCP round-trip)
   ▼
prints {"systemMessage": "📚 過去の関連教訓..."}  → injected into Claude's next turn
```

Notes:

- Direct ChromaDB call instead of going through `mcp__memory__search_lessons` (`search_hook.py:50-66`). Comment at `search_hook.py:4-5` says "low latency" — saves an MCP JSON-RPC round-trip on every user message.
- Ollama timeout is 5s here vs 15s in `server.py:49`. Different SLA: a slow hook is worse than a slow save.
- Filters: `MIN_SIMILARITY=0.45`, `MAX_RESULTS=3`, `MIN_PROMPT_LENGTH=15` (`search_hook.py:14-16`). Note the threshold is **higher** here (0.45) than the MCP tool default (0.3, `server.py:114`). The hook is more conservative because it's injected on every prompt; the MCP tool is opt-in.
- Failure mode is "transparent pass-through" — any exception in `search_hook.py:29-30`, `:45-47`, `:55-57`, `:83-85` causes `sys.exit(0)` with no output. No lessons is better than blocking the user because Ollama went down.

### 1.3 State-file as poor-man's transaction log

The repo uses a JSON file at `/tmp/claude_pending_lessons_{session_id}.json` (`pytest_tracker.py:29`) as the bridge between three independent processes: the PostToolUse hook (Python invoked by Claude Code), the Stop hook (different Python invocation), and the Claude session itself reading it via the Skill (`SKILL.md:60-65` Step 1.5). This is fragile:

- `/tmp` is not durable across reboots.
- No file locking — concurrent hook invocations could clobber.
- No schema versioning.

But it works because hook invocations are serialized per Claude session and the file is small.

---

## 2. 5-Whys + 3-sentence distillation flow

The whole flow lives in `skills/lesson-recorder/SKILL.md`. Six steps, in order:

| Step | What | Where |
|------|------|-------|
| 0 | Noise filter — skip or save? | `SKILL.md:22-39` |
| 1 | Extract error info (no raw stack traces) | `SKILL.md:42-55` |
| 1.5 | Read `/tmp/claude_pending_lessons_*.json` for cross-reference | `SKILL.md:59-65` |
| 2 | 5 Whys autonomous RCA with Grep | `SKILL.md:69-82` |
| 3 | Distill to 3-sentence template | `SKILL.md:86-107` |
| 4 | Dedup check via `search_lessons` | `SKILL.md:112-121` |
| 5 | Call `save_lesson` | `SKILL.md:125-134` |

### 2.1 The template is the contract

`SKILL.md:90-93` pins a strict template:

```
Error: [What type of error occurred and in what context — no stack traces]
Cause: [The root cause — the WHY, systemic or design issue]
Fix: [The reusable solution pattern — how to prevent this class of error]
```

Each field is **exactly 1 sentence**. The bad example (`SKILL.md:105-107`) is deliberately included to show what to reject: file paths, line numbers, "I changed line 42". The good example (`SKILL.md:97-99`) is what gets embedded — note the embeddings will be *worse* if the Skill allows verbose fields, so this discipline is load-bearing.

### 2.2 The 5 Whys question set

`SKILL.md:74-77` — four explicit questions, not literally five:

1. Why did the test fail? (technical)
2. Why was the code written that way? (design)
3. Why wasn't this caught earlier? (test gap)
4. Are there similar patterns elsewhere? → Grep

Question 4 is the *mechanized* one — the Skill explicitly tells Claude to use `Grep` for cross-codebase pattern search (`SKILL.md:77`). This is the only step that requires a tool beyond Read/Bash. Note the allowed-tools list in the frontmatter (`SKILL.md:4`): `Read, Grep, Bash, mcp__memory__save_lesson, mcp__memory__search_lessons`. No Edit, no Write — the Skill doesn't modify code, only memory.

### 2.3 Dedup threshold

`SKILL.md:117-118` — `similarity > 0.85` triggers the duplicate evaluation. This is stricter than the hook threshold (0.45) or the default MCP threshold (0.3) because we are deciding whether to *write*, not whether to *retrieve*.

### 2.4 Multi-error handling

`SKILL.md:14-19` — when the Stop hook passes multiple errors (the blocker at `stop_lesson_check.py:74-101` enumerates all resolved errors), the Skill processes them "one by one from first to last". Each gets independent Steps 0-5. This is non-trivial: the MCP doc-id uses microseconds (`server.py:160`) precisely so multi-save doesn't collide.

---

## 3. Stop-hook blocker mechanism

This is the most interesting part of the design.

### 3.1 Two-stop pattern

There are two stops in a Claude Code session: (a) Claude calls `stop` after finishing, (b) any tool call would have first fired Pre/Post hooks. The repo uses the **Stop** hook specifically — registered in `config/settings.json.example:17-22` as matcher `*`.

### 3.2 The decision tree

`claude-memory-mcp/stop_lesson_check.py:38-102`:

```
state_file exists?
  no  → approve (no pytest was tracked)
  yes ↓
parse state
  fail → approve (don't punish corruption)
  ok ↓
any resolved errors?
  no  → approve
  yes ↓
was save_lesson called this session?
  yes → approve (don't double-block)
  no  → BLOCK with systemMessage
```

Three places that exit early with approve: missing state file (`:51-53`), unparseable state (`:56-59`), no resolved errors (`:64-66`), transcript contains "save_lesson" (`:69-71`). Fail-open everywhere except the actual decision.

### 3.3 How the transcript check works

`stop_lesson_check.py:24-35` — `was_lesson_saved_this_session` reads the entire transcript file and does a substring search for `"save_lesson"`. This is a substring match, not a structural parse. Implications:

- **False positive risk:** any user message containing the literal text "save_lesson" (e.g., "how do I save_lesson a thing?") will silently unblock. Low probability but real.
- **False negative risk:** if Claude saves via a different alias or via direct tool call naming variation, the blocker fires again. The hook author relies on the Skill always invoking `mcp__memory__save_lesson` literally — and the Skill at `SKILL.md:126` does.
- **Performance:** the entire transcript is read into memory. For long sessions this could be slow. But Python `read_text` is fine for sessions under a few MB.

### 3.4 The systemMessage payload

`stop_lesson_check.py:88-94`:

```
このセッション中に N 件のテストエラーが解決されました。
停止する前に lesson-recorder スキルを使って、各エラーの教訓を長期メモリに保存してください。

【解決済みエラー一覧】
Error 1 [TypeError] @ 2026-06-26 14:32
  Tests: tests/test_db.py::test_async_connect
  Summary: RuntimeError: This event loop is already running

Error 2 ...

lesson-recorder スキルを実行し、上記の各エラーについて Step 0〜5 を適用してください。
複数エラーがある場合は、各エラーを個別に処理してください。
```

This is what Claude sees on the next turn when it tries to stop. The `systemMessage` field in a Stop hook output is the most reliable way to inject text — it goes into the conversation, not into Claude's "permission denied" reasoning.

### 3.5 Why this beats prompting

The naive approach is: tell Claude in the system prompt "please record lessons before stopping". The stop-hook approach is strictly stronger because:

1. Claude cannot skip it without the user's cooperation (the hook blocks the API call).
2. The hook knows *which* errors were tracked (via the state file) — it can list them precisely.
3. The retry loop is automatic — every stop attempt re-evaluates.

The whole design hinges on Claude Code's hook contract: a Stop hook that returns `decision: block` aborts the stop, surfaces `systemMessage` to the next turn, and lets Claude continue. This is well-documented Claude Code behavior.

---

## 4. Noise filter

Lives in two places: deterministic (in code) and soft (in the Skill prompt).

### 4.1 Deterministic filters — none

The codebase does **not** have a hard noise filter at the storage layer. The state file accepts every pytest failure (`pytest_tracker.py:76-106`). The MCP server accepts every `save_lesson` call (`server.py:148-188`). Anything rejected is rejected by the Skill's Step 0.

### 4.2 Soft filter — the Skill

`SKILL.md:26-38` — explicit skip/save criteria:

**Skip** if:
- Simple typo / indentation, fixed < 1 minute
- Same test re-run with no code changes
- Environment-specific (port, network, env var)
- Well-documented (standard `ModuleNotFoundError` → `pip install`)

**Save** if:
- Fix required non-obvious code pattern / API behavior
- Multiple attempts needed
- Root cause reveals systemic issue (missing guard, wrong type assumption)
- Fix pattern is reusable

This is a prompt-level filter. It works because:
- The Skill is invoked at a moment when Claude has just spent cycles on the fix — context is fresh.
- The four skip conditions are concrete and easy to evaluate.
- The four save conditions are concrete and require real justification.

### 4.3 What is missing

- No automated triviality detection (e.g., diff size threshold, time-on-fix threshold).
- No quality scoring on saved lessons — the 3-sentence template is enforced by prompt, not by validation.
- The hook at `pytest_tracker.py:65-66` does a coarse pre-filter: skip if tool_result has neither "passed" nor "FAILED" nor "ERROR". This catches "Bash ran `ls`" early but won't filter "Bash ran `pytest` and got 1 passed" if that's noise.

### 4.4 Storage layer is permissive

`server.py:148-188` — the `save_lesson` tool will save anything with the right shape. There's no `quality_score` field, no minimum-length check on the three sentence fields. If a bad lesson is saved, it will surface in search results and pollute future sessions. The only defense is the dedup check at `SKILL.md:112-121`, which catches near-duplicates but not low-quality originals.

---

## 5. MCP tool surface

Three tools, declared in `server.py:57-133`. All take JSON Schema input.

### 5.1 `save_lesson` — `server.py:60-94`

Required: `error_summary`, `root_cause`, `solution`. Optional: `tags` (array of strings), `project` (string). Description (`server.py:62-66`) explicitly says "Input should be pre-distilled (concise, 1 sentence per field)" — this is the contract enforced by the Skill at `SKILL.md:87`.

Implementation `server.py:148-188`:
- Builds `combined_text = f"Error: {error_summary} Cause: {root_cause} Fix: {solution}"` (`:156`).
- Embeds via Ollama (`:158`).
- Doc id `lesson_{YYYYMMDD_HHMMSS_microseconds}` (`:160`).
- Metadata stores all five fields + `saved_at` ISO timestamp (`:161-168`).
- Tags stored as JSON string because ChromaDB metadata only supports scalar values (`:165`).

### 5.2 `search_lessons` — `server.py:95-118`

Required: `query` (string). Optional: `limit` (default 5), `min_similarity` (default 0.3).

Implementation `server.py:191-232`:
- Embeds query via Ollama.
- Calls `collection.query(query_embeddings=..., n_results=min(limit, count), include=["metadatas","distances"])`.
- Converts cosine distance to similarity with `1.0 - distance/2.0` (`:213`) — comment at `:211-212` documents the math (cosine distance ∈ [0, 2] for normalized embeddings).
- Filters by `min_similarity`, formats output as `**Lesson N** [tags] (project) — saved YYYY-MM-DD (similarity: 0.NN)`.

### 5.3 `list_lessons` — `server.py:119-132`

Optional: `limit` (default 10). Implementation `server.py:235-260`:
- `collection.get(include=["metadatas"])` — fetches all metadata.
- Sorts by `saved_at` desc, takes `limit`.
- Outputs `• YYYY-MM-DD HH:MM [tags]: error_summary`.

This is a *browse* tool, not a search tool. Useful for `/mcp` ad-hoc inspection, less useful for the agent loop.

### 5.4 What's missing from the tool surface

- **No update tool** — lessons can't be edited. If the Skill saves a bad lesson, the only fix is to delete the ChromaDB entry manually.
- **No delete tool** — same.
- **No rating / quality signal** — no way for the user or a Skill to mark a lesson as unhelpful.
- **No per-project filter on search** — `project` is stored as metadata but `search_lessons` doesn't filter by it. Cross-project contamination is possible.
- **No `since` / time-window filter** on search — can't ask "lessons from the last week".

### 5.5 MCP plumbing

- `Server("memory")` instance at `server.py:25`.
- `app.list_tools()` and `app.call_tool()` decorators at `:57` and `:136`.
- Entry point `main()` at `:263-265` uses `stdio_server()` — stdio transport, no HTTP. Hooks and Claude Code both invoke it as a subprocess.
- Lazy ChromaDB init at `get_collection()` (`:32-40`) — avoids loading the DB at import time, important because `search_hook.py:52-66` and the MCP server are independent processes that both hit the same on-disk DB.

---

## 6. Slack approval gate

`claude-slack-approval/hook.py:120` — a PreToolUse hook. Not wired into `settings.json.example`; the README at `:80-88` calls it "オプション" (optional). It must be added to `hooks.PreToolUse` manually.

### 6.1 Decision flow — `hook.py:84-116`

```
tool call
   │
   ▼
is_allowed(tool_name, tool_input)?  (hook.py:36-46)
   yes → sys.exit(0)  (silent approve)
   no ↓
send_slack_approval_request  (hook.py:49-69)
   │  posts Block Kit message with two buttons
   ▼
wait_for_result  (hook.py:72-81)
   │  polls /tmp/claude_approvals/{request_id}.result every 0.5s
   │  up to 300s
   ▼
approve → sys.exit(0)
timeout → decision: block, "Slack承認がタイムアウトしました（300秒）", exit 2
deny    → decision: block, "Slackで拒否されました", exit 2
```

### 6.2 Allow-list — `hook.py:27-33`

```python
ALLOW_PATTERNS = [
    ("Bash", ["python ", "pip ", "git "]),
    ("Read", None),                              # all Reads
    ("mcp__memory__save_lesson", None),           # all saves
    ("mcp__memory__search_lessons", None),
    ("mcp__memory__list_lessons", None),
]
```

The pattern is `(tool_name, list_of_command_prefixes_or_None)`. `None` means any input for that tool is auto-approved. Anything not matching falls through to Slack.

The Bash allow-list is *prefix*-based and very narrow: only `python `, `pip `, `git `. This means `npm`, `cargo`, `pytest`, `make`, `curl` all go to Slack. For a Python-centric workflow this is OK; for any other ecosystem, this becomes noise.

### 6.3 The race / polling model

The hook writes a `request_id = str(uuid.uuid4())[:8]` (`:98`) into the Slack message (`block_id=request_id`, `action_id=approve/deny`, `value=request_id`). Then it polls `/tmp/claude_approvals/{request_id}.result` every 500ms (`POLL_INTERVAL=0.5`, `:25`) up to 300s.

But the hook.py file we have **does not contain the writer side** — there's no Slack event listener in this directory that takes the button click and writes `{request_id}.result`. This is implied to live elsewhere (perhaps in a separate Slack bot daemon). The hook is half of a two-process system; the other half is missing from this repo.

### 6.4 Slack message format — `hook.py:53-66`

Block Kit with three sections:
- Header: "Claude Code 承認リクエスト"
- Two field pairs: ツール (tool name) and リクエストID (the 8-char UUID)
- Parameter dump as fenced code block (truncated to 1500 chars at `:51-52`)
- Two action buttons: 承認 (primary, blue) and 拒否 (danger, red)

The truncation is the only defense against Slack's 3000-char block limit. Parameter dumps from large Edit/Write calls will lose information silently.

### 6.5 Operational concerns

- **Hook timeout**: `config/settings.json.example` doesn't include this hook, but a PreToolUse hook that blocks for up to 300s will stall the entire Claude Code session. Claude Code's default hook timeout is 60s — this hook will be killed unless configured higher.
- **Single-channel approval**: every approval request goes to one Slack channel. There's no per-tool or per-user routing. A noisy dev's tool calls compete with a careful reviewer's tool calls for the same channel.
- **No idempotency on re-poll**: `wait_for_result` deletes the result file after reading (`:78`). If two hook invocations share the same request_id (astronomically unlikely with UUID4), one would consume the other's result.
- **Trust boundary**: the approval directory `/tmp/claude_approvals` is writable by any local user. A malicious actor could write `{any_request_id}.result` with content `approve` to grant themselves a tool call. In practice, `/tmp` permissions prevent this on most systems but it's a real concern.

### 6.6 What this hook does NOT do

- Doesn't show diffs — for Edit/Write, only the JSON params. A reviewer can't see "this changes line 42 from X to Y" without running the command themselves.
- Doesn't show what the command would output.
- Doesn't allow partial approval ("yes to read, no to write").
- Doesn't log approved/denied decisions anywhere persistent. The `/tmp` result file is the only record, and it's deleted after consumption.

---

## 7. Pattern-worthy to steal vs. toy-quality

### 7.1 Steal-worthy patterns

**a. Stop hook as memory commit gate** (`stop_lesson_check.py`).

This is the single best idea in the repo. The pattern generalizes: any time an agent does work that should leave a durable artifact (commit message, changelog entry, ADR, runbook update, postmortem), a Stop hook can refuse to let the session end until the artifact exists. The implementation is <100 lines of Python and uses only Claude Code primitives. Same trick applies to: "did you update the changelog?", "did you add a test?", "did you tag the issue as fixed?".

**b. State file as PostToolUse sidecar** (`pytest_tracker.py` + `/tmp/claude_pending_lessons_*.json`).

The pattern of having a PostToolUse hook accumulate session state into a JSON file, then a Stop hook inspecting it, is a clean decoupling. Each hook can fail independently. The file is the contract. This generalizes to any "I need to remember what happened in this session" use case (test results, lint errors, file modifications, network requests).

**c. Embedding-on-write for retrieval, not for storage** (`server.py:156`).

Concatenating the three distilled fields into one embedding string (`"Error: ... Cause: ... Fix: ..."`) is the right call. Embedding each field separately would lose cross-field signal; embedding the full verbose form would drown the signal in noise. The distillation step (Step 3 in the Skill) is what makes retrieval work.

**d. Fail-open on hook errors** (`search_hook.py:29-30, 45-47, 55-57, 83-85`).

Every error path in the retrieve hook exits 0 silently. A broken retrieval should never block the user. This is the correct default for *advisory* hooks. (Contrast: a stop hook that fail-opens on missing state file is also correct — see `stop_lesson_check.py:51-53`.)

**e. Higher similarity threshold for retrieval-injection than for explicit search** (`search_hook.py:14` = 0.45 vs `server.py:114` = 0.3 default).

The retrieve hook is more conservative because it's injected on every prompt. Bad retrievals are noise in every subsequent turn; bad explicit searches only hurt the user when they ask. Thresholds should match blast radius.

**f. The 3-sentence template as a forcing function** (`SKILL.md:90-93`).

Requiring exactly one sentence per field forces distillation. Verbose fields would degrade embedding quality. This is a load-bearing constraint disguised as a style guideline. Reusable: any "store this as a memory entry" prompt should pin a strict template.

**g. Transcript substring check for "already done" detection** (`stop_lesson_check.py:24-35`).

Brute force but effective. Don't re-block if the user-visible evidence shows the work was done. The substring check is fragile in theory but works in practice because the Skill always invokes the tool with a literal name.

**h. Dual transport for the same DB** (MCP server for save + direct chromadb in `search_hook.py`).

The save path needs the MCP server because Claude invokes it as a tool. The retrieve path bypasses MCP for latency — a UserPromptSubmit hook should add <1s, not 2-3s for a JSON-RPC roundtrip. Two processes, one on-disk DB, one set of imports. Cheap and right.

### 7.2 Toy-quality patterns

**a. No update/delete on the MCP tool surface** (`server.py:57-133`).

Three tools, all create-or-read. There is no `update_lesson`, no `delete_lesson`, no `rate_lesson`. The first time the user saves a bad lesson, they need to manually delete a row from ChromaDB. For a "self-learning" system, this is a critical gap — bad memories are worse than no memories.

**b. Cosine similarity threshold by convention** (`server.py:114`, `search_hook.py:14`, `SKILL.md:117`).

Three different thresholds (0.3, 0.45, 0.85) hardcoded in three places. No central config. No way to tune without editing Python. For a system that *learns*, the learning rate / acceptance threshold should be a first-class knob.

**c. Transcript substring for dedup** (`stop_lesson_check.py:32-33`).

`"save_lesson" in content` — false positives on any user message mentioning the string. A 5-line JSON parse of the transcript would be more robust. Worth fixing the day someone types "save_lesson" in chat.

**d. Per-session state in /tmp** (`pytest_tracker.py:29`).

`/tmp` is volatile, world-readable on misconfigured systems, not synced. A durable session-state dir under `~/.claude/sessions/` would be better. Also: no rotation, no cap on file size. A long session with many test failures could grow the file unboundedly.

**e. Allow-list by command prefix for Bash** (`hook.py:27-33`).

`["python ", "pip ", "git "]` — hardcoded, brittle, ecosystem-specific. A more general approach: read `permissions.allow` from the existing Claude Code settings (which already has `allow`/`deny` lists at `config/settings.json.example:5-9`) and reuse it. Re-implementing a worse allow-list under a different config is anti-DRY.

**f. Slack approval via missing writer** (`hook.py:72-81`).

The repo includes the *reader* (the hook that polls for results) but not the *writer* (the Slack event listener that turns button clicks into result files). This makes the gate un-runnable out of the box. README at `:80-88` waves at it but doesn't ship it.

**g. No quality scoring on saved lessons** (`server.py:148-188`, `SKILL.md`).

A "self-learning" system that saves lessons with no quality gate and no user feedback channel will accumulate noise. There should at minimum be: (a) auto-flag short lessons as low-confidence, (b) a way to delete via MCP, (c) decay weighting (recent > old).

**h. Concatenation into single embedding loses field-level signal** (`server.py:156`).

`"Error: A Cause: B Fix: C"` as one vector means search for "fix X" might match lessons whose Cause field is similar even if the Fix is different. Three separate embeddings (or a multi-vector model) would let per-field similarity drive retrieval. The cost is 3x embedding compute, which is fine for the scale.

**i. The Skill assumes Claude will read the state file at the path the hook provided** (`SKILL.md:60-65`).

Step 1.5 says "If a session state file path was provided in the systemMessage..." — but the systemMessage format at `stop_lesson_check.py:88-94` doesn't actually embed the path; Claude has to know the convention `/tmp/claude_pending_lessons_<session_id>.json`. The session_id isn't even passed to the Skill. This works only because Claude is good at inferring the path from context, but it's brittle.

### 7.3 What to copy vs. what to redesign

If you were building a similar system today:

**Copy as-is:**
- Stop-hook-as-gate pattern.
- PostToolUse state-file pattern.
- Direct chromadb in the retrieve hook (skip MCP for latency).
- 3-sentence template with strict "1 sentence per field".
- Fail-open on advisory hook errors.

**Copy and improve:**
- Skill frontmatter (allowed-tools scoping is correct; multi-error handling is correct).
- Noise filter rules in Step 0 (the four skip conditions are good; the four save conditions are good).
- ChromaDB cosine space with HNSW.

**Redesign:**
- Add `update_lesson`, `delete_lesson`, `rate_lesson` tools.
- Move thresholds to config.
- Use a session-state dir, not `/tmp`.
- Per-field embeddings for retrieval.
- Drop the substring transcript check; parse JSONL.
- The Slack gate is a half-feature — either ship the writer or drop the gate.
- The Bash allow-list should reuse Claude Code's own `permissions.allow`.

### 7.4 Net assessment

The repo is a **working prototype** of a real idea. The core insight — *Claude cannot stop until memory is committed, and memory must be distilled to be retrievable* — is sound and broadly applicable. The vector DB plumbing is commodity. The stop hook is the load-bearing piece, and it's the smallest piece (106 lines). Everything else is glue.

Roughly 60% of the code is reusable scaffolding (MCP server, ChromaDB setup, hooks). ~25% is the actual interesting design (Stop blocker + Skill prompt). ~15% is toy-quality (transcript substring, /tmp state, no update/delete, missing Slack writer).

If you only steal one thing: **the stop-hook pattern with a state file**. It's the cheapest way to enforce "no stopping without X" and it composes with anything else you build on top.