# Claudelicious — Investigation Report

A reading of `/home/blucky/Agents/ref/claudelicious/` as a reference cookbook for building a Claude Code "harness" (the structure around the model: rules, skills, hooks, memory, agents). The repo is documentation-first plus a small set of scrubbed, copy-able files (`templates/`, `hooks/`, `skills/`, `settings/`). It is not a runnable application.

All citations use `file:line`. Where a file is short, the first number is enough.

---

## 1. The five primitives and the three principles (with file:line)

### 1.1 The five primitives ("the five words")

Defined identically in two places — the README and STORY.md agree on the vocabulary. This is the most-quoted list in the repo.

| Primitive | Definition (from the repo) | Citation |
|-----------|----------------------------|----------|
| **Model** | The LLM. "The raw intelligence. The smallest decision you make." | `README.md:70`; `STORY.md:63` |
| **Harness** | "Everything around the model. The part that compounds." | `README.md:71`; `STORY.md:64` |
| **Agent** | "A model in a harness, given a goal and the room to pursue it." | `README.md:72`; `STORY.md:65` |
| **Skill** | "A repeatable procedure the harness invokes by name. A business process, encoded." | `README.md:73`; `STORY.md:66` |
| **MCP** | Model Context Protocol. "How the harness reaches your mail, your calendar, your files, your own services." | `README.md:74`; `STORY.md:67` |

The README adds the bumper-sticker version: "A great model with no harness is a demo. A harness with a great model is a second nervous system" (`README.md:76`). STORY.md sharpens the same line into the thesis: "The model is the commodity. The harness is the moat" (`STORY.md:48`).

### 1.2 The three philosophy ideas ("PHILOSOPHY.md")

These are the load-bearing claims; everything in `docs/` is an application of one of them. The five primitives above are vocabulary; the three philosophy ideas are the *commitments*.

1. **"A system, not a chat box."** Models-of-use ladder: `Chatbot → Copilot → Agent → Harness → Dark Factory`. The repo argues rung 2 (Copilot) is the most dangerous because it feels like transformation while delivering only faster keystrokes. The cookbook covers rungs 3–5. (`PHILOSOPHY.md:7-19`)
2. **"The attention budget."** Every standing instruction costs attention on every turn; the remedy is *progressive disclosure* — the always-loaded file stays short, detail lives one layer down in `rules/*.md` or a skill's references, and you prune by relevance-per-turn and by conflict, not toward a line count. (`PHILOSOPHY.md:22-34`)
3. **"More is not better."** Compounding is keeping the ones that work and cutting the rest. The author's library was pruned from 134 to ~48 skills, and that reduction is the work, not a footnote. (`PHILOSOPHY.md:37-44`)

### 1.3 The five hook principles (different "five")

`docs/03-hooks.md:71-78` lists a separate "five principles" specific to hook design. They are worth quoting in full because they are the most actionable set in the repo:

1. **Anchor at command boundaries.** Put `(?:^|[\n;&|(])\s*` in front of any command-matching regex so the hook fires on the command, not on the string sitting inside an echo, heredoc, or replay payload. (`docs/03-hooks.md:74`)
2. **Fail open. Never wedge the tool.** Every hook wraps its logic in try/except and exits clean on any internal error. A crashing guard that blocks all Bash is worse than the narrow risk it covered. (`docs/03-hooks.md:75`)
3. **Ask, do not deny, except for two cases.** Hard-deny only (a) irreversible actions with no legitimate agent use and (b) a confirmed secret leak. Everything else asks, so the rare legitimate case survives. (`docs/03-hooks.md:76`)
4. **Ship a test suite with the hook.** Name the shapes that should fire and the ones that must not, and run them before deploying. (`docs/03-hooks.md:77`)
5. **Time injection is universal.** Every harness should do it. One line, every prompt. (`docs/03-hooks.md:78`)

These are restated in `hooks/README.md:41-52` and implemented directly in the hook scripts (see §3).

### 1.4 The flywheel

`PHILOSOPHY.md:49-68` and `STORY.md:131-153` describe the same loop. The shortest description from STORY.md:137-153:

```
You correct the agent once
   │
   ├─►  memory writes a durable note          (the reminder)
   ├─►  the learning loop writes a work order (names the file to fix)
   ▼
the broken skill / rule / agent gets edited at the source
   │
   ▼
all of it lands in the vault as plain markdown
   │
   ▼
the vault gets embedded and becomes semantically searchable
   │
   ▼
the next session starts already knowing what this one learned
```

Two supporting commitments keep the loop honest, restated in both PHILOSOPHY.md:72-74 and STORY.md:160-163: (a) **files are ground truth** (the vault is canonical plain markdown; embeddings are downstream and can be rebuilt); (b) **determinism where it matters** ("the things that must happen every time… get a hook, not a sentence").

The four-word summary appears in PHILOSOPHY.md:80 and STORY.md:215: **"Change. Measure. Decide. Repeat."**

---

## 2. Memory model and learning-loop architecture

### 2.1 Memory model — `docs/04-memory.md`

The memory model is "almost boring" by design (`docs/04-memory.md:3`): a folder of plain markdown files, one fact per file, with a one-line index, no database required to read.

**Shape of the directory** (`docs/04-memory.md:11-19`):

```
memory/
  MEMORY.md            # the index: one line per memory, loaded every session
  MEMORY-archive.md    # older index lines, moved here when MEMORY.md fills
  feedback_*.md        # corrections and lessons
  project_*.md         # current state of a piece of work
  reference_*.md       # stable facts about your tools and systems
  user_*.md            # your preferences and identity
```

**Frontmatter schema** (`docs/04-memory.md:23-30`): every file carries `name` (kebab-case slug), `description` (one line, must stand alone without the body — that's the routing signal), and `metadata.type` (one of the four values above).

**Index-not-content** (`docs/04-memory.md:37-45`): `MEMORY.md` loads every session, so it is expensive. It holds index lines, never content. Example line shown at `docs/04-memory.md:41-42` links to the memory file and gives a one-line hook. When the index grows past its ceiling, old lines move to `MEMORY-archive.md` — never deleted.

**Type taxonomy drives the body** (`docs/04-memory.md:51-68`). Four types, each with a body shape:

- `feedback` — a correction. Body is "the rule" then **Why** (the failure that produced it) then **How to apply** (prescriptive steps). The example body template is at `docs/04-memory.md:62-68`.
- `project` — the live state of a piece of work; freeform, organized by phase.
- `reference` — stable facts; dense, present tense, no Why/How needed.
- `user` — a preference or identity fact; a declarative statement.

The `feedback` convention is the one that earns its keep (`docs/04-memory.md:60`): a correction without a "why" is a sticky note that informs but does not guide.

**One fact per file** (`docs/04-memory.md:72-75`): two lessons from the same session become two files. The point is clean retirement: when a later session resolves a claim, you rewrite one file to "resolved" and update its index line. Multi-fact files can't be retired without invalidating the still-true parts.

**Wikilinks** (`docs/04-memory.md:78-81`): files cross-reference with `[[basename]]` links, by basename so they survive folder moves. The flat folder becomes a small knowledge graph; the search layer walks it.

**Curation as a real job** (`docs/04-memory.md:84-86`): without maintenance the index bloats, the system truncates it, and the most recent (often most relevant) memories get cut. A dedicated curator routine owns this. The named reader-and-router pattern that lets a session ask "what do I know about X" across memory, learnings, and rules is called `continuum` (`docs/00-the-map.md:81`).

**Named systems** (glossary at `docs/00-the-map.md:76-84`):

| Name | Role | Doc |
|------|------|-----|
| `mneme` | LanceDB-backed search over your own past sessions | `docs/07-session-search-mneme.md` |
| `continuum` | Reader + router over durable memory homes | `docs/04-memory.md:86` |
| `Pulsar` | Persistent heartbeat agent (chief of staff) | `docs/11-always-on-agents.md` |
| `Slopless` | Voice system (how the harness writes like you) | `docs/12-voice-and-antislop.md` |

### 2.2 Learning loop — `docs/05-learning-loop.md`

The learning loop is the *other* half of what fires when you correct the agent (`docs/05-learning-loop.md:7-13`):

| | Memory | The learning loop |
|---|--------|-------------------|
| Writes | a durable note | a work order |
| Says | don't repeat this | here is the exact file to edit so it cannot recur |
| Metaphor | the sticky note | the fix |

**The entry format** (`docs/05-learning-loop.md:21-28`):

```
## [LRN-YYYYMMDD-NNN] <type> | <one-line rule>
Status: pending | promoted | resolved
Cause:  skill-body | skill-trigger | skill-permission | environment
Summary: what happened and the durable rule
Promotion target: <exact file + what to change>   (or DONE: <the edit made>)
Related: <source memory file>
```

**Attribution table** (`docs/05-learning-loop.md:36-43`) — the part the repo says most loops get wrong. Classify the failure into exactly one cause, because the cause names the file *and the field*:

| Cause | What happened | The fix lands in |
|-------|---------------|------------------|
| `skill-body` | right skill fired, instructions were wrong or stale | the SKILL.md **body** |
| `skill-trigger` | wrong skill fired, or the right one stayed silent | the `description:` **triggers**, not the body |
| `skill-permission` | wrong tool access, too broad or missing | the `allowed-tools` / `model` **frontmatter** |
| `environment` | skill and trigger were fine, failure was external | **nowhere in the skill.** Log it, fix the environment. |

The last row is the trap: an external failure logged as a skill bug makes a later pass "fix" a skill that was never broken (`docs/05-learning-loop.md:43`).

**Three guardrails** (`docs/05-learning-loop.md:48-63`) so the loop doesn't rot its own files:

1. **Rejected-edit buffer.** When you veto a proposed change, log the veto. Before promoting any future edit, the loop checks the buffer first. (`docs/05-learning-loop.md:51`)
2. **Protected blocks.** Fenced invariants the loop can never auto-edit, using `<!-- protected:start reason="..." -->` and `<!-- protected:end -->` markers (`docs/05-learning-loop.md:53-61`). Examples: a dispatch-only contract, a NEVER rule, a model pin. The format is shown both in `docs/05-learning-loop.md:55-59` and reused inside `skills/post-update/SKILL.md:19-31` and `skills/designing-frontend-uis/SKILL.md:16-26,141-144`.
3. **Delta edits, not rewrites.** When a correction promotes, change the one wrong line or append the one bullet. Regenerating a section "to incorporate" a correction drives it toward shorter, blander, lossier versions (`docs/05-learning-loop.md:63`).

**Promotion order** (`docs/05-learning-loop.md:69-76`): same-session promotion is fine when the fix is mechanical and the cause is clear. Priority is: (1) the SKILL.md whose triggers/body produced the behavior, (2) the agent's identity/scope file, (3) a cross-cutting rule file, (4) the top-level config, only as a last resort. When a promotion resolves an old memory claim, rewrite that file to "resolved" and update its index line in the same pass.

### 2.3 Skill anatomy ties the two together — `docs/02-skills.md`

Skills are the centerpiece (`docs/02-skills.md:1-7`). The four things that matter in the frontmatter block (`docs/02-skills.md:58-64`):

- **`description` is the trigger surface.** Front-load the verbs and exact phrases that should fire it, and name what should *not* fire it. A vague description is the number-one cause of wrong-skill-firing or right-skill-staying-silent.
- **`model` or `agent` is mandatory.** An unpinned skill silently runs on the most expensive model every time. The model-pinning table is at `docs/02-skills.md:88-95`.
- **`allowed-tools` is the permission surface.** Give a skill what it needs and no more.
- **`disable-model-invocation`** marks a skill as user-invoked only (`/skill-name`), for anything destructive or expensive that should never auto-fire.

The progressive-disclosure pattern repeats inside a skill: short body, detail in `references/` (`docs/02-skills.md:66-77`). The dispatch-only contract — for any skill that posts/sends/publishes — is described at `docs/02-skills.md:79-82`: content must be drafted in the main session, shown, and approved *before* the skill runs.

### 2.4 The four-tool maintenance loop

`docs/02-skills.md:102-132` — skills don't stay sharp without maintenance:

1. **Mechanical linting** (`docs/02-skills.md:104`) — frontmatter validity, model/agent pinned, protected blocks balanced, description contains trigger phrases.
2. **The learning loop** — attribution as above (`docs/02-skills.md:106-115`).
3. **Guarded optimization** — protected blocks + rejected-edit buffer (`docs/02-skills.md:117-128`).
4. **Periodic consolidation** — a weekly pass reviews the library as a whole for overlap, staleness, and merges (`docs/02-skills.md:131`).

---

## 3. How the hooks integrate

### 3.1 The wiring diagram (settings.json)

The full hook table is at `docs/03-hooks.md:84-94` and the same wiring is shipped as `templates/settings.example.json:31-124` (and identically at `settings/settings.example.json:31-124` — both files are byte-identical).

| Event | Matcher | What runs | Citation |
|-------|---------|-----------|----------|
| `SessionStart` | `startup\|resume\|clear` | mint the session journal, inject the last handoff | `templates/settings.example.json:32-43` |
| `UserPromptSubmit` | empty (all) | inject `Current time: ...` | `templates/settings.example.json:45-55` |
| `PreToolUse` | `WebSearch` | append current year when query has no temporal anchor | `templates/settings.example.json:58-66` |
| `PreToolUse` | `Edit\|MultiEdit\|Write` | `block-env-edits.py` | `templates/settings.example.json:67-76` |
| `PreToolUse` | `Bash` | `block-rm-rf.py`, `injection-guard.py`, `gitleaks-pre-push.py` | `templates/settings.example.json:77-96` |
| `PostToolUse` | `Edit\|MultiEdit\|Write` | format on save (prettier/eslint/ruff/gofmt/rustfmt, all `\|\| true`) | `templates/settings.example.json:99-110` |
| `PostCompact` | `auto\|manual` | re-inject continuity state | `templates/settings.example.json:112-123` |

### 3.2 The shipped hooks

All under `hooks/`:

| File | Size | Behavior | Citation |
|------|------|----------|----------|
| `hooks/block-rm-rf.py` | 59 lines | Hard-deny `rm -rf` at command boundary; suggests `trash` | `hooks/block-rm-rf.py:31-44` (regex) |
| `hooks/injection-guard.py` | 113 lines | Ask (never deny) on four exfil/injection shapes | `hooks/injection-guard.py:58-111` |
| `hooks/block-env-edits.py` | 33 lines | Hard-deny edits to `.env`/`.env.<suffix>`, exempts `.example`/`.template`/`.sample` | `hooks/block-env-edits.py:16,21` |
| `hooks/gitleaks-pre-push.py` | 98 lines | Run gitleaks scoped to `upstream..HEAD`; deny on finding; ask on force push | `hooks/gitleaks-pre-push.py:45-97` |
| `hooks/README.md` | 59 lines | Wiring + the five hook principles | `hooks/README.md:9-13,39-52` |

### 3.3 What each hook is actually doing

**`block-rm-rf.py`** — the boundary-anchoring example (`hooks/block-rm-rf.py:31-44`). The regex starts with `(?:^|[\n;&|(])\s*` so it fires on the command, not on the string sitting inside an echo, a heredoc, or a learnings note discussing it. Matches all the common spellings (`-rf`, `-Rf`, `-fr`, `-r -f`, `-f -r`, `--recursive ... --force`, with optional `sudo`/`xargs` prefixes). Fails open on parse error (`hooks/block-rm-rf.py:22-26,57`).

**`injection-guard.py`** — the ask-layer sibling, "defense-in-depth for an autonomous, broad-allow harness" (`hooks/injection-guard.py:1-27`). Four detection shapes (`hooks/injection-guard.py:58-111`):

1. Remote content piped straight into a shell: `curl/wget | sh` (or `sudo bash`).
2. Remote content piped into a bare interpreter that executes stdin as code: `curl | python3` (negative lookahead so `python3 -c` and `python3 script.py` are excluded — piped bytes there are data).
3. `ANTHROPIC_BASE_URL=` set inline to a non-local http(s) URL — an API-key exfil vector. The `local` test at `hooks/injection-guard.py:85-88` is the *only* host-specific code (gateway naming, tailscale `100.` CGNAT, `.ts.net`) and is the part that needs scrubbing before reuse.
4. Secret source + outbound network sink in the same command. `.env` and `/environ` are deliberately excluded from the secret regex (too common in daily flow); only private keys, the password store, and cloud creds count (`hooks/injection-guard.py:96-101`). Outbound sinks exclude `ssh`/`scp`/`rsync` (authenticated, point-to-point).

Returns `ask` (human confirm) on every hit, never `deny` (`hooks/injection-guard.py:11-15`). Fails open on parse error (`hooks/injection-guard.py:48-52`).

**`block-env-edits.py`** — the simplest of the four (`hooks/block-env-edits.py:11-32`). Reads `tool_input.file_path`, regex-matches `/(^|/)\.env(\.[^/]+)?$/`, denies if matched and not in the `EXEMPT = (".example", ".template", ".sample")` allowlist. No boundary-anchoring trickery because this is a *path* match, not a command match.

**`gitleaks-pre-push.py`** — scopes to the commit range being pushed (`hooks/gitleaks-pre-push.py:34-48`). The reasoning: a `--no-git` filesystem scan flags gitignored never-pushed files (`.env.local`, `node_modules`) as false positives. Scoping to `upstream..HEAD` is fast on big repos and won't re-flag secrets already in pushed history. Behavior matrix (`hooks/gitleaks-pre-push.py:50-97`):

- gitleaks missing → `ask` with install hint
- gitleaks timeout → `ask`
- gitleaks finds → `deny` with last 1500 chars of output
- force push → always `ask`
- otherwise → pass (`{}`)

### 3.4 The inline hooks (not separate files)

Time-injection is a single shell line in `settings.json` (`templates/settings.example.json:50-52`). Format-on-save is a `case` statement dispatching by extension to eslint/prettier/ruff/gofmt/rustfmt with `|| true` so a missing tool never blocks (`templates/settings.example.json:105`). The websearch temporal-anchor appender is an inline `python3 -c "..."` script (`templates/settings.example.json:63`).

`hooks/README.md:15-17` notes this explicitly: "Time-injection and format-on-save are pure shell, so they live inline in `settings.example.json` rather than as files here."

### 3.5 Two session-manager hooks referenced but not shipped

The wiring points at `<path-to>/session-manager/session_start_hook.py` and `<path-to>/session-manager/postcompact_inject.py` (`templates/settings.example.json:38,118`). These are not in `hooks/`. The repo treats them as the operator's own continuity implementation (covered conceptually in `docs/06-continuity.md`). This is one of the "scrubbed/described but not shipped" gaps — see §5.

---

## 4. Skill library organization

### 4.1 The shape (and the discipline)

The skill library is presented as "a curated handful" — the README explicitly says "Copy the shape, not the list" (`skills/README.md:4-5`). Three archetypes, taken straight from `docs/02-skills.md` (`skills/README.md:7-16`):

| Skill | Archetype | What it shows |
|-------|-----------|---------------|
| `commit/` | encodes a process you'd do by hand | tight body, mid-tier model pin, scoped `allowed-tools` |
| `post-update/` | dispatch point for a tool you don't hand-drive | the dispatch-only contract inside a `protected:` fence, agent pin |
| `research-sweep/` | collapses recurring friction | progressive disclosure: short body + `references/` |

### 4.2 The full shipped set (8 skills, with a flagship tier)

| Skill | Lines | Key feature | Citation |
|-------|-------|-------------|----------|
| `commit/` | 41 | Process-encoder; `model: sonnet`; `allowed-tools: Bash, Read, Grep` | `skills/commit/SKILL.md:1-41` |
| `post-update/` | 46 | Dispatch-only contract in `<!-- protected:start -->` fence; `agent: cli-runner`; `allowed-tools: Bash` only | `skills/post-update/SKILL.md:19-31` |
| `research-sweep/` | 41 | Progressive disclosure: spine in body, detail in `references/`; `model: opus` | `skills/research-sweep/SKILL.md:24-32` |
| `grill-me/` | 44 | User-invoked only (`disable-model-invocation: true`); one-question-at-a-time decision-tree interrogation; `model: opus` | `skills/grill-me/SKILL.md:1-44` |
| `council/` | 86 | Subagent fan-out pattern: blind parallel seats, dissent round, single synthesis in main thread | `skills/council/SKILL.md:21-86` |
| `self-improving-agent/` | 104 | The learning loop engine; attribution taxonomy; rejected-edit buffer; `disable-model-invocation: true` | `skills/self-improving-agent/SKILL.md:30-104` |
| `dream/` | 88 | Memory consolidation pass; five checks per file; hard rule against deleting confirmed knowledge | `skills/dream/SKILL.md:38-85` |
| `designing-frontend-uis/` | 172 | Anti-slop discipline: brief-read, three dials, countable pre-flight gate, em-dash ban in `protected:` fence | `skills/designing-frontend-uis/SKILL.md:16-144` |

### 4.3 The library is curated, not bloated

The README states the original library was pruned from 134 skills to ~48 (`README.md:82`, `PHILOSOPHY.md:41`, `STORY.md:174`). The eight shipped here are a curated subset picked to demonstrate one example per archetype plus the "flagships" (skills worth reading for the *pattern*, not for direct use).

The flagships listed in `skills/README.md:19-28`:

- `grill-me/` — "the smallest useful skill"; dispatch-disabled so it never auto-fires; forces a decision tree to resolve one question at a time.
- `council/` — "the subagent fan-out pattern at its cleanest: blind parallel seats that cannot see each other, a distilled dissent round, then a single synthesis in the main thread."
- `self-improving-agent/` — "the work-order half of the learning loop… attributes a failure's root cause before picking the file to edit, treats `environment` as a terminal non-skill cause, and buffers vetoes so dead ends are not re-proposed."
- `dream/` — "memory consolidation. The proof that memory is not write-only: a scheduled pass that prunes, merges, and resolves, with a hard rule against deleting confirmed knowledge."
- `designing-frontend-uis/` — "the anti-slop discipline applied to UI: vague 'be distinctive' advice replaced with a countable pre-flight gate the model cannot pass while shipping the default."

### 4.4 The `council` skill is the cleanest subagent example

`skills/council/SKILL.md:21-86` is a full worked example of the fan-out pattern. Three phases:

- **Phase 0 — frame the decision** (`council/SKILL.md:21-28`): pick a seat slate from `slates.md`; honor `--seats` and `--decision` overrides; "orthogonality rule: never seat two lenses that collapse to the same objection."
- **Phase 1 — blind parallel reads** (`council/SKILL.md:31-43`): spawn one subagent per seat in **one message** (so they run concurrently); each seat returns under ~150 words: a read, one sharpest objection, and a 0–10 score on its `scoring_axis`. Use medium effort for this round.
- **Phase 2 — dissent round** (`council/SKILL.md:45-58`): re-spawn each seat with the *distilled* consensus (5–8 lines), never the raw transcript. Each finds where the consensus is wrong, lazy, or blind.
- **Phase 3 — synthesis in main context, not forked** (`council/SKILL.md:60-66`): the main session reconciles. Output is a decision-block: (1) where they agree, (2) sharpest live dissent, (3) the lens you were ignoring, (4) the call.

The shipped lens library is at `skills/council/lenses/` (referenced at `council/SKILL.md:80-82`): `thinkers/people/` (Munger, Taleb, Thiel), `thinkers/ideas/` (base-rates, second-order-effects), and `roles/` (the skeptic).

### 4.5 Templates organized by layer

`templates/` mirrors the architectural layers:

```
templates/
  AGENTS.md.template          # the top-of-stack context file (templates/AGENTS.md.template:1-51)
  CONTINUITY.md.template      # per-project tactical ledger (templates/CONTINUITY.md.template:1-39)
  settings.example.json       # wiring (templates/settings.example.json:1-125)
  memory/
    MEMORY.md.example         # the index file (templates/memory/MEMORY.md.example:1-23)
    feedback_TEMPLATE.md      # body schema with Why/How (templates/memory/feedback_TEMPLATE.md:1-14)
    project_TEMPLATE.md
    reference_TEMPLATE.md
    user_TEMPLATE.md
    examples/                 # sanitized example files showing the shape per type
  learnings/
    LEARNINGS.md.template     # the work-order log (templates/learnings/LEARNINGS.md.template:1-25)
    ERRORS.md.template
    REJECTED.md.template      # the negative-feedback buffer
  rules/
    security.md               # example rule file (templates/rules/security.md:1-45)
    model-selection.md
    web-scraping.md
```

The repeated pattern: templates show the *shape*, never real content. `templates/memory/MEMORY.md.example:11` calls this out explicitly: "The lines below are sanitized examples showing the shape, one per type."

`settings/settings.example.json` and `templates/settings.example.json` are byte-identical (verified by read). The duplicate is worth flagging — see §6.

---

## 5. What is copyable vs opinionated

### 5.1 Directly copyable (the repo says "ship as-is")

Each doc ends with a "Ship / scrub" section. The patterns and templates the author explicitly says are generic:

- **The five hook principles** (`docs/03-hooks.md:100-102` and `hooks/README.md:39-52`): boundary anchoring, fail-open, ask-not-deny (with two exceptions), test suite, universal time injection. The four shipped Python hooks are scrubbed and ready to wire (`hooks/README.md:54-58`).
- **The time-injection hook** — literally one `echo` line (`templates/settings.example.json:51`). The format-on-save dispatch by extension is also generic.
- **The skill anatomy and frontmatter contract** (`docs/02-skills.md:135-138`): pin a model/agent, scope allowed-tools, front-load the description with triggers and exclusions, use `disable-model-invocation: true` for destructive skills.
- **The dispatch-only contract** (`docs/02-skills.md:79-82`) — encode it as `<!-- protected:start -->` fence at the top of any skill that posts/sends/publishes.
- **The skill-bibliography-cut discipline** (`docs/02-skills.md:135-138`, `PHILOSOPHY.md:37-44`): prune by relevance-per-turn and by conflict, not toward a line count.
- **The memory schema and taxonomy** (`docs/04-memory.md:91-93`): `MEMORY.md` index file + `feedback_*.md` / `project_*.md` / `reference_*.md` / `user_*.md` + frontmatter with name/description/type + Why/How body for feedback + wikilinks + `MEMORY-archive.md` rollover.
- **The learning-loop format and the four-cause attribution table** (`docs/05-learning-loop.md:87-89`): ship the format, ship the templates, don't ship the log.
- **The three guardrails** (`docs/05-learning-loop.md:48-63`): rejected-edit buffer, `<!-- protected:start -->` blocks, delta edits over rewrites.
- **The AGENTS.md hierarchy and progressive disclosure** (`docs/01-rules-and-context.md:73-75`): user-global → machine-local → project, `AGENTS.md` is the canonical filename with `CLAUDE.md` as a symlink. The template at `templates/AGENTS.md.template:1-51` is the copy-paste starting point.
- **The CONTINUITY.md pattern** (`docs/06-continuity.md` referenced from `templates/CONTINUITY.md.template:1-39`): Done / Now / Next / Decisions / Open questions.
- **The Pulsar agent skeleton** (`docs/11-always-on-agents.md:60-70`): timer + classifier shell + heartbeat routing file + SOUL.md + CONTEXT.md + silence as default. The *anatomy* is generic, the *contents* of SOUL/CONTEXT are personal.
- **The anti-slop discipline applied to a domain** (`skills/designing-frontend-uis/SKILL.md:160-170`): "A rule you can count beats a rule you can rationalize."

### 5.2 Opinionated / non-copyable as-is

These are described as patterns but depend on the operator's environment:

- **The injection-guard allowlist heuristics** (`hooks/injection-guard.py:84-88`): the `local` test is host-specific — your gateway name, your tailnet range, your hostname convention. The README says scrub these before sharing (`hooks/README.md:54-58`).
- **All session-manager Python scripts** (`templates/settings.example.json:38,118`): the wiring points at `<path-to>/session-manager/session_start_hook.py` and `postcompact_inject.py`, but no implementation is shipped. The repo describes what they do (`docs/06-continuity.md`) but you build your own.
- **The agent named systems** (`docs/00-the-map.md:76-84`): `mneme` (LanceDB), `Pulsar` (heartbeat chief of staff), `continuum` (memory router), `Slopless` (voice system). `AGENTS.md:23` says they are "patterns to learn from, not packages to install blindly."
- **Personal memory files**: `MEMORY.md`, `MEMORY-archive.md`, and `user_*.md` files by definition hold personal data (`docs/04-memory.md:93`). Never publish.
- **The five-machine mesh** (`README.md:82`, `STORY.md:184`): a laptop + a GPU box + a couple of always-on nodes. The shape is general but the actual fleet isn't.
- **The eight named agents** (`STORY.md:185`): a *small staff* of agents with roles + memory. The cost claim ("less than the cost of a couple of lunches a month") is operator-specific.
- **The "70,000 vault documents" scale** (`README.md:82`, `STORY.md:183`): the four-tier memory taxonomy generalizes; the absolute scale does not.
- **`designing-frontend-uis/SKILL.md:166-168`** is a worked anti-example from the operator's own TTS voice-grading lab — it's a cautionary tale, not a template.

### 5.3 The repeated meta-pattern

What is repeatedly copyable across the repo: **make rules countable, make enforcement deterministic, make authority explicit**. Each of the most-recommended pieces satisfies at least one of these:

- Time injection: deterministic (`UserPromptSubmit` hook, can't be skipped).
- `rm -rf` block: deterministic, fail-open, boundary-anchored.
- The dispatch-only contract: enforceable because it's in a `protected:` fence the learning loop cannot edit.
- The em-dash ban (`skills/designing-frontend-uis/SKILL.md:141-144`): countable — "a single one anywhere is a pre-flight fail."
- The skill pruning discipline: the `description:` field is the trigger surface, and the trigger phrases are countable.
- The five hook principles: each is a rule you can audit a hook against.

What is *not* copyable is the *content* of any of these: your identity (`user_*.md`), your project's state (`project_*.md`), your corrections (`feedback_*.md`), your lessons (`LEARNINGS.md`), your agent's `SOUL.md`. The schema is the product; the contents are yours.

---

## 6. Honest critique

This is the part where I am not going to sugar-coat it. The cookbook is well-crafted and useful, but it has limits worth naming out loud.

### 6.1 What it does well

- **It has a point of view.** "More is not better" is a thesis, not a feature list, and the skill library, the hooks, and the memory schema are all designed to *enforce* that thesis mechanically (progressive disclosure, protected blocks, delta edits, rejected-edit buffer). Most "here is my setup" repos are a flat inventory; this one is a design with teeth.
- **The vocabulary is locked first.** The five primitives (Model / Harness / Agent / Skill / MCP) are defined identically in README.md:69-75 and STORY.md:62-67. Vocabulary before architecture; that discipline shows up everywhere downstream.
- **The hooks are concrete and tested by the design.** Boundary-anchoring with `(?:^|[\n;&|(])\s*` (`hooks/block-rm-rf.py:31-33`) is a small, sharp detail that turns a false-positive-prone string match into a usable guard. The fail-open posture (`hooks/block-rm-rf.py:22-26`, `hooks/injection-guard.py:48-52`) is the right default — a wedge-on-Bash hook is worse than the narrow risk it covered.
- **The learning loop's attribution table is the strongest idea in the repo.** Four causes, each mapping to a specific file *and field*. `environment` as a terminal non-skill cause (`docs/05-learning-loop.md:43`) is the line that separates a working loop from one that "fixes" skills that were never broken.
- **The cookbook earns its `ship / scrub` section.** Every doc ends with one. The author is explicit about what is generic and what is personal, which is rare.

### 6.2 What is genuinely weak

- **`docs/05-learning-loop.md:18`**: the entry template uses `LRN-YYYYMMDD-NNN` but `skills/self-improving-agent/SKILL.md:67` uses `LRN-YYYYMMDD-XXX`. The format placeholder disagrees in two shipped files. Minor, but the kind of inconsistency that bites when you build tooling.
- **`templates/settings.example.json` and `settings/settings.example.json` are byte-identical** (verified by read; both end at line 125 with the same `PostCompact` block). One is a duplicate. Either move one to point at the other or delete the duplicate; the cookbook's own anti-bloat discipline applies here.
- **Two referenced Python scripts are not shipped.** `<path-to>/session-manager/session_start_hook.py` and `<path-to>/session-manager/postcompact_inject.py` (`templates/settings.example.json:38,118`) are placeholders that the wiring requires. `docs/06-continuity.md` describes what they should do but there is no implementation. The cookbook tells you what to build, not how. That's defensible (it's a cookbook, not a starter kit) but the user has to know they're getting half the picture.
- **The "150 experiments overnight" claim in `STORY.md:204-211`** is offered as the payoff of the autonomy layer. It is a single anecdote with no citation, no setup, no baseline. It is doing rhetorical work, not evidentiary work. Take it as a story, not a result.
- **The cookbook over-quotes itself.** STORY.md repeats PHILOSOPHY.md almost verbatim. The same flywheel ASCII art appears in `PHILOSOPHY.md:51-68`, `STORY.md:136-153`, and `docs/00-the-map.md:7-54`. The same five-hook-principles list appears in `docs/03-hooks.md:71-78` and `hooks/README.md:39-52`. The same "134 → 48 skills" line appears in `README.md:82`, `PHILOSOPHY.md:41`, `STORY.md:174-181`, and `docs/02-skills.md:1-7`. This is a cookbook, so some repetition is intentional — but it's heavy enough that you could compress 30–40% of `STORY.md` and lose nothing.
- **The `designing-frontend-uis/SKILL.md` is 172 lines and the README's "anti-slop" claim is genuinely useful, but the em-dash ban (`skills/designing-frontend-uis/SKILL.md:141-144`) is presented as universal** when it is the operator's house style. It is in a `protected:` fence, which means the learning loop will not soften it — but a user copying the skill wholesale inherits a stylistic choice that is opinion, not engineering.
- **`injection-guard.py` ships with a real-looking tailnet heuristic** (`hooks/injection-guard.py:86-88`: `host.startswith("100.")`, `host.endswith(".ts.net")`). `hooks/README.md:54-58` flags this as host-specific. But the *exemption* list (`SECRET_STRICT` excluding `.env` at line 96-101) is opinionated: a different operator's flow reads `.env` in a context that is genuinely an exfil shape, and this guard will miss it. The trade-off is documented, but it's still a judgment call baked into the code.
- **The five principles (philosophy) and the five primitives (vocabulary) and the five hook principles and the four archetypes of skills and the four-cause attribution and the four-tool maintenance loop** — the number-five pattern is doing a lot of rhetorical work. It's catchy, but it sometimes forces bad groupings. The "five primitives" is genuine; the "five hook principles" is also strong; the "more is not better" being labeled one of three ideas is fine. But "five words" in `STORY.md:56` and "five primitives" in `README.md:64` and "five principles" in `docs/03-hooks.md:70` are three different "fives" that the reader can easily confuse.
- **No tests for the hooks are shipped.** `docs/03-hooks.md:77` says "Ship a test suite with the hook… A routing hook with twenty named cases catches the false positive before it derails a real session." The shipped hooks do not include those test suites. The principle is correct; the implementation is missing.

### 6.3 What is missing entirely

- **No example memory files with real content.** `templates/memory/examples/` is referenced but not read in this pass — and `templates/memory/MEMORY.md.example:11` only shows three sanitized index lines. A worked `feedback_*.md` with a real-shaped "Why / How to apply" body would teach the discipline faster than the template does.
- **No example `LEARNINGS.md` entries.** `templates/learnings/LEARNINGS.md.template:1-25` shows the format, but a worked entry that demonstrates the `Cause` → `Promotion target` reasoning would be more convincing than the schema.
- **No model-pinning rules in `templates/rules/`.** `docs/02-skills.md:88-95` has the model-pinning table, but `templates/rules/` has `model-selection.md` as a placeholder — not a worked example. The user has to derive the policy.
- **No worked `SOUL.md` or `CONTEXT.md` for the Pulsar pattern.** `docs/11-always-on-agents.md:50-56` describes them; nothing in `templates/` ships one. Same critique as the missing session-manager scripts.
- **No worked example of the flywheel actually closing.** The narrative in `PHILOSOPHY.md:49-68` and `STORY.md:131-163` is a one-pass diagram. A single concrete session where "you correct → memory writes → learning loop writes → skill gets edited → next session reads it back" would prove the loop more than the diagram does.

### 6.4 Bottom line

Claudelicious is a strong reference for someone who already knows they want a harness and wants the design vocabulary + the wiring pattern + the boundary-anchoring trick + the attribution table. It is a *terrible* "clone and run" starter kit, and it knows it (`README.md:88-91`). The biggest gap is asymmetry: the *patterns* are well-explained, but the *shipped implementations* are partial (no session-manager scripts, no example memory or learnings entries, no worked SOUL/CONTEXT, no hook tests). If you adopt it, plan to write the missing pieces yourself — and to scrub `injection-guard.py` to your own network.

The strongest portable ideas, in order: progressive disclosure (PHILOSOPHY.md:22-34), the learning-loop attribution table (docs/05-learning-loop.md:36-43), the boundary-anchored `rm -rf` regex (hooks/block-rm-rf.py:31-44), the protected-block contract (docs/05-learning-loop.md:53-61), and the dispatch-only contract for skills that post (skills/post-update/SKILL.md:19-31). Those five carry the cookbook. The rest is reinforcement.