# Memory Production Systems — Open Source Landscape

> Survey of open-source memory/learning systems relevant to an automated research lab.
> Focus: systems that **produce and evolve** memory automatically (not just retrieve it).

**Context:** The user is building an automated research lab (Rust + Turso + AI agents)
and has already investigated:
- `ref/continuous-learning-v2/` — instinct-based learning (already reported)
- `ref/claude-mem/` — vector + FTS5 memory with session hooks
- `ref/MiMo-Code/` — Claude Code memory system with reconciliation
- `ref/Co-Scientist/` — Python reference for the co-scientist pattern

This report covers systems **beyond that set** that the user may want to evaluate,
steal patterns from, or integrate. Categorized by approach, with honest assessment.

---

## 1. Three Tiers of Memory Production Systems

The space clusters into three distinct design philosophies:

| Tier | Philosophy | Examples |
|------|-----------|----------|
| **Tier 1 — Observation hooks → derived patterns** | Watch everything the agent does, mine patterns periodically, evolve into reusable artifacts | continuous-learning-v2 (already known), **ai-agent-os**, **claude-self-learning-loop**, **levelup-skill** |
| **Tier 2 — Explicit memory layer as state** | Memory is a first-class object the agent reads/writes via tools, with LLM-driven extraction/consolidation | **mem0**, **LangMem**, **Letta (MemGPT)** |
| **Tier 3 — Self-organizing memory graph** | Memories are nodes in a graph; LLM continuously rewrites links and metadata based on new content | **A-MEM** |

There is also a meta-tier: **cookbooks/harnesses** that don't ship a memory system
but document how to compose one. These are the highest-signal learning material
even when their code is small.

---

## 2. Tier 1 — Observation & Pattern Mining Systems

These are most directly comparable to `continuous-learning-v2`.

### 2.1 `Vadim2090/ai-agent-os` — *Second-Brain OS for Claude Code*

**URL:** https://github.com/Vadim2090/ai-agent-os  
**Stars:** 2  
**Lang:** Shell 77%, Python 23%  
**License:** MIT

**The interesting bits:**

- **4-file memory model** (not a JSONL stream):
  - `focus.md` — active strategic streams (loaded at session start)
  - `inbox.md` — GTD capture buffer (counter-only at start, body on demand)
  - `references.md` — stable IDs/URLs/tokens (never auto-loaded)
  - `sessions-history.md` — append-only timeline (only top entry prepended via shell)

- **Hybrid markdown + SQLite** — narrative in MD, structured data in SQL.
  Explicit rule: "If you'd put it in a spreadsheet, it belongs in the DB.
  If you'd write it as a paragraph, it stays in markdown."

- **Three-tier execution architecture** (this is the lab-relevant pattern):
  - Tier 1: Interactive (Claude Code session, every action visible)
  - Tier 2: Supervised Autonomous (cron + Slack/Telegram gates for approval)
  - Tier 3: Fully Autonomous (scheduled scripts, watchdog on failure)
  - Promotion path: validated manual → cron + Slack review → silent with alerts only

- **Self-learning loop**: user correction → hook captures → queued → `/reflect`
  proposes `CLAUDE.md` update → user approves. (Same shape as clv2's
  `/evolve` but human-in-the-loop at the final step.)

- **Skill extraction**: `/claudeception` skill — turns non-obvious discoveries
  into reusable skills in `~/.claude/skills/`.

- **Hooks as mechanical enforcement** (`learning-activator.sh`,
  `content-guard.sh`, `finish-staleness-check.sh`) — agent behavior is
  enforced by code, not docs.

- **Lessons from three months of iteration** (the honest retrospective
  embedded in the README — worth reading):
  1. Killed `handoff.md` — duplicated `sessions-history.md`, caused torn writes.
  2. Moved tasks out of memory — they accumulated with "carried x21"
     counters. Real task tracker is the answer.
  3. Streams as session-start anchor, not to-do list.
  4. Operational data → SQLite, narrative → markdown.
  5. `/finish` to shell-prepend (not model-rewrite) — 5 min → 1.5 min on
     200 KB file.

**Honest assessment:** This is what `continuous-learning-v2` would look like if
built by someone who shipped it for 3 months and iterated. The README alone
is worth the read — the lessons section is exactly the kind of "things I wish
I'd known" content missing from clv2.

**Relevance to your lab:** The three-tier execution pattern is directly
applicable — supervisor/worker pipeline already exists in co-scientist; the
Slack/Telegram approval gates map cleanly to the agent loops. The 4-file
memory model is also worth borrowing for the lab's own orchestration layer.

---

### 2.2 `TOMTOM2004/claude-self-learning-loop` — *Lesson Recorder via MCP*

**URL:** https://github.com/TOMTOM2004/claude-self-learning-loop  
**Stars:** 0 (new)  
**Lang:** Python 100%  
**License:** MIT

**Architecture diagram (paraphrased from README):**

```
[pytest fails] → Claude fixes → [pytest passes]
                          ↓
         PostToolUse hook (pytest_tracker.py) writes state file
                          ↓
         Stop hook (stop_lesson_check.py) blocks if lesson unsaved
                          ↓
         lesson-recorder Skill runs
         (noise filter + 5 Whys RCA + 3-sentence distillation)
                          ↓
         mcp__memory__save_lesson → Ollama embedding → ChromaDB

[User prompt] → UserPromptSubmit hook (search_hook.py)
                          ↓
         Direct ChromaDB query (low latency)
                          ↓
         Related lessons injected as systemMessage
```

**The interesting bits:**

- **State file as a forcing function** — `pytest_tracker.py` records
  fail→success transitions; `stop_lesson_check.py` blocks session end if
  there are unsaved lessons. The Stop hook is the critical enforcement
  mechanism.

- **5 Whys RCA + 3-sentence distillation** in the recorder skill.
  This is **better engineering than clv2's instinct extraction** —
  clv2 just trusts Haiku to write "good" YAML, this pipeline explicitly
  forces the model through a structured post-mortem.

- **Noise filter before save** — solves the "low-quality instinct pollution"
  problem clv2 has (see report §9 limitation #5).

- **MCP server as memory backend** (3 tools: `save_lesson` / `search_lesson`
  / `list_lesson`) — clean separation of memory ops from agent ops.

- **ChromaDB + Ollama embeddings (local)** — no external API dependency,
  runs offline.

**Honest assessment:** Small (3 commits), no traction (0 stars), and
Japanese-only README. But the architecture is **technically correct** in a
way clv2 isn't — it has explicit gates (Stop hook block), explicit
distillation (5 Whys), and a noise filter. The README even reads like a
tutorial. Worth stealing the 5-Whys-distill pattern for your lab.

**Relevance to your lab:** If you add a reflection/distillation step
between observation and instinct creation in clv2 (or build equivalent in
co-scientist), you'd close the gap. The Stop-hook-as-blocker pattern is
the highest-leverage idea here.

---

### 2.3 `huketo/levelup-skill` — *Curated learnings.md as compounding memory*

**URL:** https://github.com/huketo/levelup-skill  
**Stars:** 1  
**Lang:** Shell 100%  
**License:** MIT

**The interesting bits:**

- **A single file (`learnings.md`) as the entire memory** — five sections:
  - Consolidated Principles (standing rules, no dates)
  - Patterns That Work (dated)
  - Mistakes to Avoid (dated)
  - Domain Knowledge (dated)
  - Open Questions (dated)

- **SessionStart hook auto-injects** the file as context. Compaction-safe.

- **Three lifecycle commands:**
  - `init-learnings` — scaffold from template
  - `update-learnings` — append atomic dated entries
  - `consolidate-learnings` — **the evolution step**. Prunes stale, merges
    duplicates, promotes recurring patterns to Consolidated Principles.

- **Explicit inclusion bar:** *"Would a future session behave meaningfully
  differently if it knew this? If no, leave it out."*

- **Consolidation is what makes the loop compound.** Without it the file
  bloats and crowds the context window. The author is explicit: "a bloated
  learnings.md is worse than none."

**Honest assessment:** The opposite philosophy from clv2: clv2 writes many
small atomic files, levelup writes one well-curated file. levelup is what
clv2 would evolve into if you cared more about quality than quantity. The
**consolidation step** is the missing piece in clv2's evolution pipeline —
clv2 has `/evolve` (cluster → generate) but no equivalent of "merge and
distill".

**Relevance to your lab:** The "consolidated principles" concept maps well
to your co-scientist's prompt-template system — those templates ARE
consolidated principles for each agent type. The Open Questions section
is interesting for hypothesis generation.

---

### 2.4 `marcelloromanelli/harness` — *Cross-harness operator core*

**URL:** https://github.com/marcelloromanelli/harness  
**Stars:** 0  
**License:** MIT (vendored from affaan-m/ECC)

**The interesting bits:**

- **Two-repo model** — public (harness) + private (ecc overlay).
  Per-skill symlinks managed by `skill-sync`. Solves the "private data in
  public skills" problem that bit clv2 when ECC first published.

- **42 skills + rules + agents + zero-dep memory/learning hooks**.
  The **memory layer uses continuous-learning-v2's instinct store** but
  surfaces top instincts at session start. So this is "clv2 with
  session-start injection" — fills the gap that clv2's "open and read
  manually" UX has.

- **Plugin/cowork marketplace** via `.claude-plugin/plugin.json` — auto-
  updates on git push. Solves the distribution problem.

- **Cursor + Cowork compatibility** — same hooks work across harnesses.
  Uses symlinks rather than per-harness copies.

**Honest assessment:** Thin wrapper around ECC (clv2). The two-repo model
and the session-start injection are the contributions. If you're going to
distribute co-scientist as a public repo, study the `skill-sync` pattern.

**Relevance to your lab:** Probably **low direct relevance** — co-scientist
is a research tool, not a developer harness. But the "consume clv2's
instincts at session start" pattern is worth noting.

---

## 3. Tier 2 — Explicit Memory Layer (Production-Scale)

These are full memory products, not Claude Code plugins. They have
adoption, benchmarks, and papers. Most relevant for the **lab-level**
memory question (how does an agent system as a whole remember).

### 3.1 `mem0ai/mem0` — *Universal memory layer for AI Agents*

**URL:** https://github.com/mem0ai/mem0  
**Stars:** 59.5k (huge)  
**License:** Apache 2.0  
**Paper:** arXiv:2504.19413

**The interesting bits:**

- **Single-pass ADD-only extraction** (April 2026 algorithm). One LLM call,
  no UPDATE/DELETE. Memories accumulate; nothing is overwritten. **This
  is the opposite of clv2's "update existing instincts" approach.**

- **Multi-signal retrieval** scored in parallel and fused:
  - Semantic (vector)
  - BM25 keyword
  - Entity matching (entity linking across memories)
  - Temporal reasoning (time-aware ranking)

- **LoCoMo: 91.6, LongMemEval: 94.8, BEAM (1M): 64.1** — published
  benchmarks, reproducible framework.

- **Three scopes:** User / Session / Agent state. Multi-level memory with
  adaptive personalization.

- **Production deployment:** managed cloud, self-hosted Docker, OSS library.
  Real production story.

- **Agent Skills distribution:** ships `npx skills add` reference skills
  (`mem0`, `mem0-cli`, `mem0-vercel-ai-sdk`) and pipeline skills
  (`mem0-integrate`, `mem0-test-integration`). Shows the right way to ship
  memory infra to AI coding agents.

**Honest assessment:** This is the **state of the art** for agent memory as
a managed layer. The benchmark numbers are serious. The April 2026
algorithm change (ADD-only, no UPDATE/DELETE) is interesting — it's a
**deliberate trade** that prioritizes recall over consistency. If your
lab needs to remember research progress across long horizons, this is the
production option.

**Relevance to your lab:** **High.** If co-scientist needs long-term
research memory (across sessions, across projects), mem0 is the
drop-in option. The agent-skills distribution pattern is also the right
model for distributing your lab's tools to Claude Code / Cursor / etc.

**Caveat:** mem0 is opinionated about extraction — it stores facts, not
patterns. If you want clv2-style "patterns observed 11 times", mem0 won't
do that. The two systems are complementary.

---

### 3.2 `langchain-ai/langmem` — *LangChain's memory primitives*

**URL:** https://github.com/langchain-ai/langmem  
**Stars:** 1.5k  
**License:** MIT

**The interesting bits:**

- **Three APIs in one library:**
  1. **Core memory API** — works with any storage system (functional primitives)
  2. **In-conversation tools** (`create_manage_memory_tool`,
     `create_search_memory_tool`) — agent decides when to store/search
     during a conversation
  3. **Background memory manager** — automatic extraction, consolidation,
     update (the "evolving" part)

- **Native integration with LangGraph** — `BaseStore` is the persistence
  layer. Postgres-backed for production, in-memory for dev.

- **The "hot path" pattern** — agent uses search tool during a turn.
  No background job needed for recall.

- **Background memory manager** — `create_memory_manager(...)` runs as
  a separate process. This is the closest analog to clv2's observer-loop:
  a background LLM that reads new conversations and updates memory.

**Honest assessment:** Less traction than mem0 but cleaner architecture.
The separation of "in-conversation tool use" vs "background consolidation"
is the right decomposition. If you're building co-scientist from scratch,
the API surface of langmem (4-5 functions) is what a Rust equivalent should
look like.

**Relevance to your lab:** **Medium.** Worth looking at if you want
to build a custom memory layer in Rust (rather than call mem0's API).
The `create_memory_manager` background pattern is exactly what clv2's
observer-loop.sh tries to do — but langmem's version is cleaner.

---

### 3.3 `letta-ai/letta` (formerly MemGPT) — *Stateful agents with self-improving memory*

**URL:** https://github.com/letta-ai/letta  
**Stars:** 23.5k (huge)  
**License:** Apache 2.0

**The interesting bits:**

- **Memory blocks as first-class state.** Not facts, not chunks — labeled
  blocks the agent reads and edits. `human` and `persona` are default
  blocks. The agent has tools to read/write its own memory blocks.

- **Tiered memory** — core/in-context, archival (vector DB), recall (recent
  messages). Memory pressure → compaction.

- **Self-improvement** — the agent can rewrite its own memory blocks during
  a conversation. The system prompt explicitly tells the agent to update
  memory when it learns something.

- **Letta Code CLI** — runs agents locally with skills + subagents +
  bundled memory skills. **This is the "ship the agent, not just the lib"
  philosophy.**

**Honest assessment:** Different philosophy from clv2/mem0. Letta treats
memory as **the agent's own scratchpad**, not an external store the agent
queries. Tradeoff: better at persona/preference tracking, worse at scale
(human's memory block is small).

**Relevance to your lab:** **Medium-low.** Letta is built for
conversational agents with personas, not research agents. But the
"agent has tools to edit its own memory" pattern is interesting for
co-scientist's supervisor agent — let it write its own state.

---

## 4. Tier 3 — Self-Organizing Memory Graphs

### 4.1 `agiresearch/A-mem` — *Zettelkasten for LLM agents*

**URL:** https://github.com/agiresearch/A-mem  
**Stars:** 1.1k  
**License:** MIT  
**Paper:** arXiv:2502.12110

**The interesting bits:**

- **Zettelkasten principles** — each new memory:
  1. Generates comprehensive structured attributes (tags, context, keywords)
  2. Analyzes historical memories for relevant connections
  3. Establishes meaningful links based on semantic similarity
  4. **Continuously evolves** — every add/update rewrites related memories'

- **Automatic evolution.** When you add a new memory, the system:
  - Searches ChromaDB for related memories
  - Updates their tags/context based on the new memory
  - Creates new semantic connections

- **Single Python class** (`AgenticMemorySystem`) — note/create/read/search/
  update/delete/evolve. Very small API surface.

- **Benchmarked against SOTA baselines** on six foundation models.

**Honest assessment:** The **most interesting evolution mechanic** in this
entire report. A-MEM's "add a memory → it rewrites other memories" is what
makes it truly *agentic* — the memory isn't a passive store, it's a
self-organizing knowledge graph.

But: it's also **expensive** (LLM call per add to evolve neighbors), and
the "rewrite neighbors on add" pattern can cause **catastrophic forgetting**
in long-running systems unless versioned carefully. No mention of how
A-MEM handles conflicting rewrites.

**Relevance to your lab:** **High, with caution.** If co-scientist needs
to build a knowledge graph of hypotheses/observations that reorganizes
as new evidence comes in, A-MEM's pattern is the right starting point.
But implement versioning + conflict resolution before scaling.

---

## 5. Cookbooks & Meta-Tier (Pattern Sources)

These don't ship a memory system — they document how to compose one.

### 5.1 `BioInfo/claudelicious` — *Claude Code cookbook (homelab-focused)*

**URL:** https://github.com/BioInfo/claudelicious  
**Stars:** 3  
**License:** MIT (code) + CC BY-NC 4.0 (docs)

**The interesting bits:**

- **"The model is the commodity. The harness is the moat."** — frames the
  whole problem correctly. Memory lives in the harness, not the model.

- **The five primitives:** Model / Harness / Agent / Skill / MCP. Simple
  vocabulary for everything else.

- **19 docs, each one principle + example + ship-or-scrub notes:**
  - 04 Memory
  - 05 The learning loop
  - 06 Continuity (post-compaction)
  - 07 Session search (mneme)
  - 08 The second brain
  - 15 Loops and autonomy

- **130 skills deliberately pruned to "a curated few dozen"** — the cut is
  the craft.

- **4-tier memory taxonomy** over ~70k plain-markdown vault documents.
  Scale matters: this system actually runs at scale, not just demoed.

- **Dual-licensed**: code in `templates/`, `hooks/`, `skills/` is MIT;
  docs in `docs/` are CC BY-NC 4.0. You can fork the code, not the prose.

**Honest assessment:** This is the highest-signal repo in this report.
The README alone contains more architectural wisdom than most books.
Even if you never use any of the code, the vocabulary (`focus.md`,
`inbox.md`, `streams`, `loops`, `second brain`) is worth adopting.

**Relevance to your lab:** **High for thinking, low for direct use.**
The "5 principles + 3 pillars" framing and the "one source of truth per
concept" rule are applicable to co-scientist's memory architecture
decisions (which tool owns which data).

---

## 6. Comparative Matrix

| System | Type | Storage | Evolution | Feedback Loop | Maturity |
|--------|------|---------|-----------|---------------|----------|
| continuous-learning-v2 (your ref) | Hook-based | YAML files | `/evolve` clusters → skills | None (write-only) | Production (yours) |
| claude-mem (your ref) | MCP/hook | Chroma + SQLite | Session-start injection | None | Production (yours) |
| ai-agent-os | Hook + slash cmds | 4 MD files + SQLite | `/reflect` + `/claudeception` | Yes (manual `/reflect`) | Personal use (3 months) |
| claude-self-learning-loop | MCP + hooks | ChromaDB | 5-Whys distillation in skill | Yes (Stop-hook block) | Toy (0 stars, 3 commits) |
| levelup-skill | Hook + slash cmds | Single MD file | `consolidate-learnings` | Implicit (consolidation) | Personal use |
| mem0 | Memory layer SDK | Postgres + Qdrant | Background extraction | Implicit (per-user) | Production (59.5k ★) |
| langmem | Memory primitives | Any BaseStore | Background manager | Yes (background) | Production (1.5k ★) |
| Letta (MemGPT) | Agent platform | DB + blocks | Agent rewrites own memory | Yes (in-conversation) | Production (23.5k ★) |
| A-MEM | Memory graph | ChromaDB | Automatic neighbor evolution | Implicit (per-add) | Research (1.1k ★) |

**Legend:** ★ = GitHub stars; "Production" = actively shipped and used.

---

## 7. What Your Lab Should Steal (Prioritized)

If I were designing memory for co-scientist, in priority order:

### 7.1 Adopt the **5-Whys distillation pattern** from `claude-self-learning-loop`

clv2 trusts Haiku to write good instincts. clv2 has no stop-hook
blocker. clv2 has no noise filter. Adding a structured "5 Whys RCA
→ 3-sentence distillation → noise filter → save" pipeline between
observation and instinct-write would dramatically increase instinct
quality with marginal cost.

**Cost:** Add a Skill that the observer calls before writing. ~50 LOC.

### 7.2 Add **consolidation step** from `levelup-skill`

clv2 has `/evolve` (cluster → generate skill) but no equivalent of
"merge duplicates → distill to standing principle". Adding
`/consolidate` that merges similar instincts and promotes recurring
patterns to a "Standing Principles" file would close the gap clv2's
SKILL.md documents but never implements.

**Cost:** Add `cmd_consolidate` to `instinct-cli.py`. ~100 LOC.

### 7.3 Adopt the **four-file memory model** from `ai-agent-os`

co-scientist currently uses `~/.local/share/mimocode/memory/` with
multiple scopes (global/sessions/tasks). Adopting the explicit
separation of **focus / inbox / references / history** would force
clear ownership and prevent the "carried x21" anti-pattern (accumulating
stale tasks because nothing prunes them).

**Cost:** Refactor memory layout. ~200 LOC + migration script.

### 7.4 Evaluate **mem0 for research memory** if/when scale demands

Once co-scientist accumulates >10k hypotheses/observations across
sessions/projects, the JSONL + inverted-index approach in co-scientist's
`memory.rs` will start to feel slow. mem0's hybrid retrieval
(semantic + BM25 + entity + temporal) at 91.6 LoCoMo is the production
option. Would integrate as an MCP server.

**Cost:** External dependency. Decide based on actual scale hit.

### 7.5 Study **A-MEM's self-rewriting neighbor pattern** — but don't ship it yet

A-MEM's "adding memory X rewrites memory Y" is the most interesting
evolution mechanic in the field. But it has unresolved conflict
resolution. Worth prototyping for co-scientist's
hypothesis-evolution chain (does new evidence rewrite related
hypotheses?), but not as the primary memory model.

**Cost:** Research prototype. ~2 weeks to evaluate.

### 7.6 Adopt **session-start memory injection** from `levelup-skill` / `harness`

clv2 writes instincts but doesn't **inject** them into the session.
`/instinct-status` is manual. Adding a SessionStart hook that
auto-loads the top N instincts (sorted by confidence) into the
agent's context would make clv2 actually useful.

**Cost:** ~30 LOC hook.

### 7.7 Read `claudelicious` docs for vocabulary, not code

The 19 docs in `BioInfo/claudelicious` are the best architectural
writing on memory-as-harness-component available. The 5 primitives
+ 5 principles framing is directly applicable to co-scientist's
design discussions.

**Cost:** Reading time.

---

## 8. What NOT to Adopt

### 8.1 Don't adopt Letta's "agent edits own memory" pattern

Letta is built for conversational personas. co-scientist is a
multi-agent research loop with strict separation of concerns
(supervisor / worker / reflection / ranking). Letting agents edit
their own memory blocks would make the pipeline non-reproducible.

### 8.2 Don't adopt A-MEM's automatic neighbor rewriting wholesale

A-MEM's rewriting-everything-on-add is great for small knowledge
graphs, catastrophic at scale. The "every memory rewrites its
neighbors" pattern needs versioning + conflict resolution that
A-MEM doesn't ship.

### 8.3 Don't try to merge clv2 + mem0 + langmem into a Frankenstein

Each of these has its own opinionated data model. Trying to merge
them creates an unmaintainable mess. Pick the one that matches
your access pattern (YAML instincts for clv2's pattern case,
mem0 for conversation-recall case) and use it.

### 8.4 Don't trust star counts as quality signals

Mem0 has 59.5k stars but its April 2026 algorithm is **deliberately
dumber** than its predecessor (ADD-only, no UPDATE) — and the
README frames this as a feature. Stars measure popularity, not
correctness for your use case.

---

## 9. Systems Considered and Rejected

| System | Reason |
|--------|--------|
| `affaanmustafa/homunculus` (ECC origin) | 404 on GitHub — original deleted; clv2 is the evolved fork |
| `ifxprime/kodelyth-ecc` | Variant of ECC, redundant |
| `SideMountain/claude-code-sidekick` | Boilerplate skill template, no memory production |
| `AnotherSava/claude-code-common` | Personal config repo, no evolution |
| `DimaTimoschenko02/claude-code-kit` | Skill snippets, no system |
| `lucasrudi/token-optimizer-memory` | Prompt efficiency, not memory production |
| `starlucasrudi/token-optimizer-memory` | Same as above |
| Other ECC derivatives | Redundant |

Note: search hit 429'd on multiple queries (mem0, agentic memory) —
there are likely 20+ more systems in this space I couldn't reach.
The above is a representative sample, not exhaustive.

---

## 10. Final Recommendation

For the automated research lab, **do not adopt any single system wholesale**.
Instead:

1. **Keep clv2 for instinct capture** (it works for hook-based observation).
2. **Add 2 missing pieces from this survey:**
   - 5-Whys distillation (from `claude-self-learning-loop`)
   - Consolidation step (from `levelup-skill`)
3. **Add session-start injection** to make instincts actually load.
4. **Evaluate mem0** when/if co-scientist hits research-memory scale pain.
5. **Document** your memory architecture in the style of `claudelicious` —
   "the harness is the moat", and your memory is the most important
   piece of harness.

The most valuable thing in this survey isn't any specific system — it's
the **vocabulary and trade-off catalog**. You now have a map of the
design space (Tier 1 / Tier 2 / Tier 3, observation vs explicit vs
self-organizing). Pick deliberately, not by star count.

---

## Sources

- https://github.com/Vadim2090/ai-agent-os
- https://github.com/TOMTOM2004/claude-self-learning-loop
- https://github.com/BioInfo/claudelicious
- https://github.com/huketo/levelup-skill
- https://github.com/marcelloromanelli/harness
- https://github.com/mem0ai/mem0 (arXiv:2504.19413)
- https://github.com/langchain-ai/langmem
- https://github.com/letta-ai/letta
- https://github.com/agiresearch/A-mem (arXiv:2502.12110)
- https://github.com/search?q=claude+code+memory+skill+hook+learning

*Generated 2026-06-26. Survey focused on **memory production** systems
beyond the user's existing references (continuous-learning-v2, claude-mem,
MiMo-Code, Co-Scientist).*
