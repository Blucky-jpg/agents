# Investigation Report: `ai-agent-os` — Vadim2090's Personal AI Operating System

**Source repo**: `/home/blucky/Agents/ref/ai-agent-os/`
**Investigated**: 2026-06-26
**Scope**: architectural patterns worth lifting from the reference repo. No sugar-coating.

`ai-agent-os` is a folder-and-skill harness layered on Claude Code. It introduces a
**4-file session memory model**, a **3-tier autonomy graduation path**, a **two-stage
self-learning loop** (capture via hooks → reflect via skill), a **skill-extraction engine**
("claudeception"), and **mechanical enforcement** via three shell-script hooks. The README
calls itself a "Personal AI Operating System" (`README.md:1`) — and the most useful part is
the "Lessons From Three Months In" section (`README.md:313-321`), where the author owns
five real design failures. The system is portable in spirit (markdown core) but the wiring
depends on Claude Code's hook events (`settings.json.template:15-55`).

---

## 1. The 4-file memory model

### 1.1 What the four files are

`README.md:73-86` defines the canonical four, with `wip.md` and `meetings.md` as opt-in extras:

| File | Loaded at `/start`? | Purpose | Citation |
|------|---------------------|---------|----------|
| `memory/focus.md` | yes | Active strategic streams. The portfolio view. | `README.md:79`, `template/memory/focus.md:1-23` |
| `memory/sessions-history.md` | yes — top entry only, sliced via `awk` | Append-only timeline; top entry = "last session" | `README.md:80`, `START.md:21-32` |
| `memory/inbox.md` | counter only | GTD capture buffer. Never auto-loaded. | `README.md:81`, `template/memory/inbox.md:1-13` |
| `memory/references.md` | no | Stable IDs/URLs/tokens. On-demand only. | `README.md:82`, `template/memory/references.md:1-17` |

Plus `memory/wip.md` (created by `/checkpoint`, cleared by `/finish` — `skills/checkpoint/SKILL.md:12-43`)
and `memory/meetings.md` (distilled only — raw meetings stay in Granola/Otter —
`template/memory/meetings.md:1-10`).

### 1.2 The load rules ARE the design

`README.md:73`: *"Each file has one purpose. No duplication."* `START.md:10-41` enforces it:
load `focus.md` + top-of-history + (conditional) `wip.md`; `inbox.md` only via
`grep -c "^- "` for a count (`START.md:45-49`); `references.md` never. The principle is
"load the minimum, on demand." The comment at `template/MEMORY.md:3-5` makes the token
argument explicit: "lines after 200 will be truncated by auto-memory."

### 1.3 The single load-bearing rule

`README.md:317`: *"Killed `handoff.md`. It duplicated the top entry of `sessions-history.md`.
Two files claiming to be 'the latest' = torn-write race conditions when sessions ran in
parallel."* This is reinforced at `skills/finish/SKILL.md:23`: "/finish only writes that one
file." The whole architecture rests on **one source of truth per concept**.

### 1.4 Tasks are NOT in memory

`README.md:86`: *"Tasks do not live in memory. They live in your real task tracker (Notion,
Linear, Asana, etc.). Memory holds the strategic portfolio — what streams you're operating
on this sprint. Tasks ≠ focus. Different cadence, different consumer, different tool."*
Repeated four times across `template/CLAUDE.md:66,90,122,128` because it gets violated most.

### 1.5 MEMORY.md is a pointer-only index

For the Claude Code built-in auto-memory layer: `README.md:95` — *"MEMORY.md must be a pure
pointer index — no inline data. Every line costs tokens on every turn. Data lives in topic
files, loaded only when relevant."* Same warning at `template/MEMORY.md:3-5` with a 200-line
truncation limit. This is pure token-economics reasoning.

### 1.6 Honest read

**Steal-worthy**: one-purpose-per-file; the `awk` slice for history top entry
(`START.md:21-32`); the "tasks live elsewhere" rule.
**Fragile**: assumes a Notion/Linear-style tracker; no schema for stream Active → Paused →
Dormant (`template/memory/focus.md:10-23`); one `wip.md` can't handle three parallel sessions.

---

## 2. The three-tier agent architecture

### 2.1 The tiers

`README.md:111-124`:

```
Tier 1: Interactive                    You + AI agent, real-time
Tier 2: Supervised Autonomous          Agent runs alone, pauses at checkpoints
Tier 3: Fully Autonomous               Scheduled scripts, no human needed
```

Four design rules at `README.md:127-130`: start at Tier 1; every Tier 3 needs a watchdog;
skills are portable across tiers; demote back if quality drops.

### 2.2 The example path

`README.md:133-137` — `/system-health` skill (`skills/system-health/SKILL.md:1-25`) graduated
over three weeks from manual → cron-with-Slack-review → cron-with-failure-alerts-only.

### 2.3 What promotion actually means

`README.md:271-278`: extract logic to a standalone script (Python/Node), add Slack/Telegram
checkpoint gates, deploy to a server with cron, add a watchdog, monitor for quality.

### 2.4 Honest read

**Steal-worthy**: "start at Tier 1" (rule 1) prevents scheduling unvalidated automations;
"demote back if quality degrades" (rule 4) is a healthy permission most systems don't grant.
**Hand-waved**: the SKILL.md → standalone-script refactor isn't mechanical — anything the
agent does via Read/Grep/TodoWrite isn't trivially re-implementable in a cron; no cost
model at any tier (a Tier 3 cron that calls an LLM per run can silently cost more than the
human time it saved).

---

## 3. The self-learning loop

### 3.1 The two stages

`README.md:141-144`: capture-on-prompt → queue → user runs `/reflect` → propose CLAUDE.md
update → user approves. `skills/claude-reflect/SKILL.md:11-16` confirms: Stage 1 automatic
(capture), Stage 2 manual (process).

### 3.2 The capture hook — what it matches

`skills/claude-reflect/scripts/capture_learning.py:19-37` lists the regexes:
- **High confidence**: `\bno,?\s+(use|always|never|don't|do not)\b`, `\bactually[,.]`,
  `\binstead of\b`, `\buse\s+\w+\s+not\s+\w+`, `\bremember:`, etc.
- **Medium confidence**: `\bthat's\s+(wrong|incorrect|not right)\b`, `\bprefer\s+\w+\s+over\b`,
  `\bstop\s+(using|doing)\b`, `\bwe\s+(use|prefer|need)\b`.

Queue file at `~/.claude/learnings-queue.json` (line 16). Each entry: timestamp, raw message
(truncated to 500 chars), confidence, matched_pattern, suggested_scope (heuristic at
line 88: `"project"` if cwd contains `/projects/`, else `global`), working_dir (lines 86-97).

### 3.3 /reflect procedure and destinations

`skills/claude-reflect/SKILL.md:51-61` — read queue, propose changes per entry, get
approval, apply, clear processed items. Destinations at `skills/claude-reflect/SKILL.md:46-49`:
- `~/.claude/CLAUDE.md` — Global learnings
- `./CLAUDE.md` — Project-specific
- `./CLAUDE.local.md` — Personal (gitignored)
- `./.claude/rules/*.md` — Modular rules with path-scoping

### 3.4 Reflect vs claudeception

The split worth noting: **reflect** updates instructions (CLAUDE.md) with behavioral
corrections; **claudeception** creates new skills (SKILL.md) from procedural knowledge. Both
run post-work, but the artifact is different: reflect = "use X not Y"; claudeception =
"here's a 10-step diagnostic flow for when Z fails."

### 3.5 Honest read

**Steal-worthy**: the two-stage capture/process design — auto-apply would be dangerous;
the 4-way destination split avoids the "everything in one giant CLAUDE.md" failure mode.
**Broken in current state**: `capture_learning.py` exists but isn't wired into
`settings.json.template` (only `learning-activator.sh` is, at line 21). The self-learning
loop **doesn't work out of the box**. Other issues: no dedup (5 corrections = 5 queue
entries); the scope heuristic at line 88 is naive (a non-AI-OS folder named
"my-projects" would falsely match); no skill-retirement mechanism.

---

## 4. Skill extraction (`claudeception`)

### 4.1 The trigger surface

`skills/claudeception/SKILL.md:1-9` — three trigger modes: explicit `/claudeception`,
natural-language ("save this as a skill"), or implicit after non-obvious discovery. The
third is what the `learning-activator` hook nudges toward.

### 4.2 The quality gate

`skills/claudeception/SKILL.md:27-33` — four checks before extraction:
- **Reusable**: Will this help with future tasks?
- **Non-trivial**: Requires discovery, not just documentation lookup?
- **Specific**: Can you describe exact trigger conditions and solution?
- **Verified**: Has this solution actually worked?

### 4.3 The skill template and cap

`skills/claudeception/SKILL.md:46-71` — standard SKILL.md structure (Problem / Context /
Solution / Verification / Notes). The `description:` field is what the auto-picker matches
against, and the instruction is to be precise about triggers. Retrospective mode at
`skills/claudeception/SKILL.md:77-85` caps extraction at **1-3 skills per session** — a
sensible discipline that prevents skill-spam.

### 4.4 Honest read

**Steal-worthy**: the four quality criteria; the 1-3-per-session cap; the explicit
split between "instruction update" (reflect) and "skill creation" (claudeception).
**Opinionated**: the 1-3 cap is arbitrary and doesn't scale with session length; no usage
tracking means dead skills stay forever; no retirement mechanism.

---

## 5. Hooks as mechanical enforcement

### 5.1 The principle

`README.md:179`: principle #3, "Mechanically enforce structure — hooks > written rules."
`template/knowledge-base/ai-agent-principles.md:14` sharpens it: *"Don't just write rules
— build automated checks that physically prevent violations."* Three hooks ship
(`hooks/`, wired in `settings.json.template:15-55`):

| Hook | Event | Purpose |
|------|-------|---------|
| `learning-activator.sh` | `UserPromptSubmit` | Remind agent to evaluate for extractable knowledge |
| `content-guard.sh` | `PostToolUse` (Write, Edit) | Scan output for banned words/phrases |
| `finish-staleness-check.sh` | `SessionStart` | Warn if last session was >24h ago |

### 5.2 `learning-activator.sh` — soft enforcement

`hooks/learning-activator.sh:12-28` emits a ~17-line heredoc banner on every prompt
("SKILL EVALUATION REMINDER" + 3 questions + "if YES use /claudeception"). This is **prompting
via the system**, not enforcing — the banner appears every turn regardless of context.
Across a long session that's real overhead for what is essentially "be aware of /claudeception."

### 5.3 `content-guard.sh` — real enforcement (sort of)

`hooks/content-guard.sh:20-24` defines an empty `BANNED_PATTERNS` array for the user to
populate. The logic:
- Lines 28-30: no-op if no patterns configured.
- Lines 33-44: parse stdin, extract `tool_input.file_path` via Python.
- Lines 47-49: skip if no file path or file missing.
- Lines 52-55: scan only `.md .txt .js .py .html .json .ts .jsx .tsx`.
- Lines 58-63: grep each pattern; ignore `#` lines and `BANNED` mentions.
- Lines 65-72: print violations banner.
- Line 74: `exit 0` — **does not block the Write**, just warns.

Good details: filters config-file comments (line 59), caps each match to 3 (line 59).
Missing: no fail-closed mode; no path-scoped exemption.

### 5.4 `finish-staleness-check.sh` — clean hygiene gate

`hooks/finish-staleness-check.sh:13-42` — simplest of the three:
- Line 13: `HISTORY_FILE="${AI_OS_PATH:-$HOME/AI OS}/memory/sessions-history.md"`
- Lines 23-27: `stat -c %Y` (Linux) vs `stat -f %m` (macOS) — the author thought about
  portability, which most hobby projects skip.
- Line 30: age in hours.
- Lines 33-40: if >24h, print "SESSION HYGIENE" banner.

The cleanest enforcement example. One nit: `setup.sh:84-89` does a one-time `sed` to bake
the install path in — moving the dir later leaves a stale path. A runtime check would be
more robust.

### 5.5 Honest read

**Steal-worthy**: `finish-staleness-check.sh` is a textbook example — simple, fails open,
single behavior. `content-guard.sh`'s `BANNED_PATTERNS` array is a clean customization point.
**Over-stated**: the README's "physical prevention" claim (`ai-agent-principles.md:14`) is
not what the hooks do — all three exit 0 unconditionally. They're advisory, not preventive.
**Cost concern**: `learning-activator.sh` adds ~17 lines of context per turn; a
frequency-limit would be a cheap optimization. **Portability**: hooks are locked to Claude
Code's hook events, so the "portable to any agent" claim in `README.md:3` is conditional.

---

## 6. The "Lessons From Three Months In" section

`README.md:313-321` is the most useful section in the README. Five confessions of design
failures.

### 6.1 Killed `handoff.md` — single source of truth

`README.md:317`: "Two files claiming to be 'the latest' = torn-write race conditions when
sessions ran in parallel." **Steal-worthy** as a general principle — "if two files claim the
same thing, delete one."

### 6.2 Moved tasks out of memory

`README.md:318`: *"Tasks accumulated in handoff.md with carry counters going up to 'carried
x21'. Items at x14+ aren't tasks — they're a museum of work never killed."* The signal:
"if carry count > ~3, you have a process bug." **Steal-worthy.**

### 6.3 Switched to streams as the /start anchor

`README.md:319`: at session start you need orientation (what initiatives this week?), not
enumeration (all my todos?). Streams serve orientation; tasks serve enumeration. Different
needs, different files. **Steal-worthy** — but only if you actually have streams.

### 6.4 Moved operational data to SQLite

`README.md:320`: markdown can't answer "show me leads who replied but never booked." The
clean version at `README.md:106-107`:

> If you'd put it in a spreadsheet, it belongs in the DB.
> If you'd write it as a paragraph, it stays in markdown.

**Steal-worthy.** Note: `template/` doesn't ship a concrete SQLite schema — `data/` is
empty in the template.

### 6.5 Refactored /finish to shell-prepend

`README.md:321`: *"When the model is rewriting a 200 KB file every wrap, /finish takes 5+
minutes. When the model writes only the new entry and a shell op prepends, /finish takes
~1.5 min."* The implementation at `skills/finish/SKILL.md:48-61`:
1. Agent writes `/tmp/new_history_entry.md`.
2. Shell op finds the first `## ` heading, uses `head -n $((SPLIT-1))` + entry +
   `tail -n +$SPLIT` to rebuild the file.
3. Atomic `mv .tmp target`.

**Most steal-worthy pattern in the repo.** General rule: **don't have the agent
stream-replace a large file when a shell splice will do.**

### 6.6 The unspoken sixth lesson

`README.md:86` (in the memory model section, not the lessons section): "Tasks do not live
in memory." This IS the lesson — the section just makes the concrete failure (x21 counters)
visible. The principle was always there; the lesson was that without it, memory becomes
a junk drawer.

### 6.7 Honest read

The five lessons are real failures with real fixes. The implicit "kill the second source
of truth" rule is general. **What's missing**: no metric on cost — "5 minutes wasted" is
anecdotal, not measured; no "what we tried that didn't work either" — the section is
post-hoc rationalization, not a debugging journal. Knowing what *other* designs the author
tried and abandoned would be more useful.

---

## 7. Pattern-worthy to steal vs opinionated

### 7.1 Steal-worthy (general, portable)

1. **One purpose per memory file** — adapt the four to your work; you might not need `meetings.md` (`README.md:73`).
2. **Tasks ≠ focus** — streams go in memory; tasks go in a tracker (`README.md:86`).
3. **Shell-slice, don't full-load** — `awk` for top entry of the session log (`START.md:21-32`).
4. **Shell-prepend large append-only logs** — agent writes to temp, shell splices. Avoids the 200 KB rewrite tax (`skills/finish/SKILL.md:48-61`).
5. **Pointer-only MEMORY.md** — always-loaded memory holds only pointers; details in topic files loaded on demand (`README.md:95`).
6. **Tier-1-first workflow promotion** — every automation starts manually (`README.md:127`).
7. **Hooks-as-defaults, not blocking gates** — emit warnings, exit 0; lower risk of agent getting stuck (`hooks/`).
8. **Hybrid markdown + SQLite** — "If you'd put it in a spreadsheet, it belongs in the DB" (`README.md:99-107`).
9. **Capture-then-process learning** — auto-detect corrections, queue them, process manually. Never auto-apply (`README.md:141-144`).
10. **Skill quality gate** — Reusable / Non-trivial / Specific / Verified. Force every extracted skill to pass all four (`skills/claudeception/SKILL.md:27-33`).

### 7.2 Opinionated (works for this author, may not for you)

1. **`focus.md` as orientation anchor** — only useful if you have multi-week streams (`README.md:79`).
2. **Notion/Linear as the task tracker** — no "tracker-less" mode is offered (`README.md:86`).
3. **Hook wiring via `settings.json.template`** — locked to Claude Code's hook events (`settings.json.template:15-55`).
4. **`AI OS/` as the home directory root** — a folder called `AI OS/` (with a space) in `~` is a strong opinion (`README.md:23-36`).
5. **Meeting-tool integration** — assumes Granola/Otter/etc. If you don't use one, `meetings.md` is dead weight (`README.md:158-163`).
6. **Rigid `/start` / `/finish` protocol** — breaks for ad-hoc quick questions or batch scripting (`README.md:62-69`).
7. **Two separate learning systems** — `reflect` + `claudeception` could be one unified flow (`README.md:141-152`).
8. **3-tier autonomy model** — useful mental model, but the SKILL.md → script refactor is hand-wavy (`README.md:271-278`).

### 7.3 Broken or fragile in current state

1. **`capture_learning.py` not wired in** — `settings.json.template:16-25` doesn't list it; self-learning loop doesn't work out of the box.
2. **No skill-retirement mechanism** — once created, a SKILL.md stays forever.
3. **No dedup in learning queue** — `capture_learning.py:99-101`; same correction 5 times = 5 entries.
4. **Hooks don't block** — `hooks/content-guard.sh:74` `exit 0` always. "Physical prevention" claim (`ai-agent-principles.md:14`) is overstated.
5. **`learning-activator.sh` adds context every turn** — `hooks/learning-activator.sh:12-28`; 17 lines × N turns = real cost.
6. **`setup.sh` does one-time `sed`** — moving the install dir later leaves stale paths (`setup.sh:84-89`).
7. **`ONBOARD.md` assumes patient user** — "verify each step" (line 21) breaks down if half the setup is already configured (`ONBOARD.md:21-25`).

### 7.4 What you'd build differently from scratch

- Wire `capture_learning.py` by default and dedupe by content hash.
- Replace `learning-activator.sh` with a turn-rate-limited reminder (e.g., every 5 turns).
- Add `/prune-skills` for skills unused in N sessions.
- Make `content-guard.sh` actually block on critical patterns (exit 2) and warn on soft ones.
- Replace `AI OS/` (with space) with `~/ai-os` or similar.
- Add a "first session" onboarding hook that detects missing config and walks the user through it.
- Ship a concrete SQLite schema (currently `data/` is empty in `template/`).
- Add a `/watchdog` skill that monitors Tier 3 cron jobs.

---

## 8. Appendix — file-by-file map

| Path | Load-bearing? | Why |
|------|---------------|-----|
| `README.md` | Yes (orientation) | 4-file model + 3-tier model live here |
| `ONBOARD.md` | No (UX script) | Paste-into-chat guide; replaceable |
| `setup.sh` | Yes (install) | Paths, hooks, skills wiring |
| `settings.json.template` | Yes (wiring) | Hook events that make enforcement work |
| `hooks/learning-activator.sh` | No (advisory) | Adds context every turn; not critical |
| `hooks/content-guard.sh` | Yes (when configured) | Real banned-pattern enforcement |
| `hooks/finish-staleness-check.sh` | Yes (hygiene) | Best example of mechanical enforcement |
| `skills/start/SKILL.md` | Yes (trivially) | Just delegates to `START.md` |
| `skills/finish/SKILL.md` | Yes | The shell-prepend pattern lives here |
| `skills/checkpoint/SKILL.md` | Marginal | Useful for parallel sessions |
| `skills/claudeception/SKILL.md` | Yes | Quality gate + SKILL.md template |
| `skills/claude-reflect/SKILL.md` | Yes | Two-stage learning model |
| `skills/claude-reflect/scripts/capture_learning.py` | Yes (but unwired!) | The actual capture logic |
| `template/CLAUDE.md` | Yes | Behavioral contract |
| `template/START.md` | Yes | Shell-slice `awk` lives here |
| `template/MEMORY.md` | Yes | Pointer-only rule lives here |
| `template/memory/focus.md` | Yes | Stream schema |
| `template/memory/inbox.md` | Marginal | GTD buffer — only loads if asked |
| `template/memory/references.md` | Marginal | On-demand only |
| `template/memory/sessions-history.md` | Yes | Kill-`handoff.md` lesson lives here |
| `template/memory/meetings.md` | Niche | Only useful with a meeting tool |
| `template/knowledge-base/ai-agent-principles.md` | Yes | The 5 principles + 3 pillars |

---

## 9. Bottom line

`ai-agent-os` is a real product that has survived three months of iteration with a real
user. The "Lessons From Three Months In" section (`README.md:313-321`) is worth more than
the rest of the README combined — it's where the author owns what actually broke.

The sharpest single idea is **one source of truth per concept** (`README.md:229`):

> If two files claim to be "the latest", one is wrong. If tasks live in your task tracker
> AND in memory, you're paying for both and reconciling neither.

Everything else is downstream of that. Steal that first.

The second-sharpest is **don't have the agent stream-rewrite a large file when a shell
splice will do** (`README.md:321`, `skills/finish/SKILL.md:48-61`). The 3.3× speedup on
`/finish` (5 min → 1.5 min on 200 KB) is real, and the pattern applies anywhere you have
an append-only log.

The third is **tasks don't live in memory, streams do** (`README.md:86,319`). The "carried
x21" failure mode (`README.md:318`) is a real failure — anyone who's used a markdown todo
list for more than a month has seen it.

The repo's biggest weakness: the self-learning loop (`capture_learning.py`) isn't wired in
by default, so the headline feature ("the agent gets better over time") doesn't work out
of the box. Fix that one thing and the system becomes meaningfully more compelling.

---

*End of report. 9 sections. Citations throughout. Realistic about what's load-bearing vs overhead.*