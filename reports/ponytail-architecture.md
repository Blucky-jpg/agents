# Ponytail — Architecture & Integration Report

> Reference: `ref/ponytail/` (upstream: `DietrichGebert/ponytail`, version 4.8.3).
> License: MIT. Author: Dietrich Gebert.

## 1. What ponytail is

Ponytail is a **portable agent skill** that injects a "lazy senior dev" ruleset
into the system / system-prompt context of an AI coding agent. It is not a
tool, not an LSP, not a code generator — it is a **prompt-injection layer**
plus a small command vocabulary.

The product is a single rule, in seven rungs, plus the wiring to land it in
sixteen different agent hosts:

```
1. Does this need to exist?   → no: skip it (YAGNI)
2. Already in this codebase?  → reuse it, don't rewrite
3. Stdlib does it?            → use it
4. Native platform feature?   → use it
5. Installed dependency?      → use it
6. One line?                  → one line
7. Only then: the minimum that works
```

`AGENTS.md:1-32` is the compact, host-agnostic form. `skills/ponytail/SKILL.md:1-117`
is the full form with intensity tiers and worked examples. Both files assert
the same invariants (see §11).

The measured effect, per the agentic benchmark at `benchmarks/results/2026-06-18-agentic.md`
(12 tickets on `tiangolo/full-stack-fastapi-template`, Haiku 4.5, n=4): **−54% LOC,
−22% tokens, −20% cost, −27% time, 100% safety** vs the same agent with no skill.

## 2. Repository layout

```
ref/ponytail/
├── AGENTS.md                 # compact always-on rules (32 lines, all hosts)
├── __init__.py               # Hermes plugin entrypoint (register(ctx))
├── plugin.yaml               # Hermes plugin manifest (hooks/commands/skills)
├── package.json              # npm @dietrichgebert/ponytail (exports → .opencode/plugins/ponytail.mjs)
├── opencode.json             # OpenCode config: plugin → ./.opencode/plugins/ponytail.mjs
├── gemini-extension.json     # Gemini extension manifest (contextFileName=AGENTS.md)
├── .claude-plugin/
│   ├── marketplace.json      # Claude Code marketplace manifest
│   └── plugin.json           # Claude Code plugin manifest (hooks → claude-codex-hooks.json)
├── .codex-plugin/plugin.json # Codex plugin manifest
├── .devin-plugin/plugin.json # Devin CLI plugin manifest
├── hooks/                    # SHARED LIFECYCLE HOOKS (Node, CommonJS)
│   ├── ponytail-config.js          # mode resolution + isShellSafe + deactivation phrase
│   ├── ponytail-instructions.js    # SHARED instruction builder + skill-body filter
│   ├── ponytail-runtime.js         # flag-file I/O + per-host stdout shaper
│   ├── ponytail-activate.js        # SessionStart hook
│   ├── ponytail-subagent.js        # SubagentStart hook
│   ├── ponytail-mode-tracker.js    # UserPromptSubmit hook
│   ├── ponytail-statusline.sh      # POSIX statusline (badge in chat)
│   ├── ponytail-statusline.ps1     # Windows statusline
│   ├── claude-codex-hooks.json     # Claude + Codex hook manifest
│   └── copilot-hooks.json          # Copilot CLI hook manifest
├── skills/                   # SHARED SKILL LIBRARY (six skills, one SKILL.md each)
│   ├── ponytail/SKILL.md           # core lazy-mode ruleset (the ruleset, 117 lines)
│   ├── ponytail-review/SKILL.md    # over-engineering review (one-liner findings)
│   ├── ponytail-audit/SKILL.md     # whole-repo audit variant
│   ├── ponytail-debt/SKILL.md      # harvest ponytail: comments → ledger
│   ├── ponytail-gain/SKILL.md      # measured-impact scoreboard
│   └── ponytail-help/SKILL.md      # quick reference card
├── commands/                 # Codex slash-command TOMLs (loaded by Codex plugin)
│   └── ponytail*.toml
├── .opencode/
│   ├── plugins/ponytail.mjs  # OpenCode server plugin (ES module)
│   └── command/*.md          # OpenCode slash commands (frontmatter + template)
├── pi-extension/
│   ├── index.js              # pi package extension
│   └── test/                 # pi extension tests
├── ponytail-mcp/             # MCP server (stdio, prompt + tool)
│   ├── index.js              # McpServer wiring
│   ├── instructions.js       # thin re-export over hooks/ponytail-instructions
│   ├── package.json          # @modelcontextprotocol/sdk ^1.26, zod ^3.23
│   └── test/instructions.test.js
├── .cursor/rules/            # Cursor project rule (.mdc)
├── .windsurf/rules/          # Windsurf project rule
├── .clinerules/              # Cline project rule
├── .agents/rules/            # generic AGENTS-style rule
├── .kiro/steering/           # Kiro steering rule
├── .openclaw/skills/         # OpenClaw skill packages (generated; see §9)
├── .github/copilot-instructions.md  # GitHub Copilot instruction file
├── scripts/                  # build/check/publish helpers
│   ├── check-rule-copies.js  # invariant check across AGENTS.md ↔ copies ↔ SKILL.md
│   ├── check-versions.js     # version-pinning lint across manifests
│   ├── build-openclaw-skills.js     # generate .openclaw/skills/ from skills/
│   ├── publish-openclaw-skills.js   # publish six skills to ClawHub
│   └── uninstall.js          # remove flag/config/statusline state outside the plugin
├── docs/
│   ├── agent-portability.md  # adapter ↔ host matrix (canonical reference)
│   └── platform-native.md    # "you don't need this package" cheat sheet
├── examples/                 # 12 worked before/after survivors (csv-sum, debounce, …)
└── tests/                    # 10 node:test files (212-line hooks suite alone)
```

Two design facts to internalize before reading further:

1. **The shared builder is `hooks/ponytail-instructions.js::getPonytailInstructions`.**
   Every host that injects on every turn calls this function. There is exactly
   one source of truth for the ruleset body. The Codex hook, the Claude hook,
   the OpenCode plugin, the pi extension, the Hermes plugin, the MCP server,
   and the fallback path all route through it.
2. **The shared config resolver is `hooks/ponytail-config.js`.** Mode resolution
   order is `PONYTAIL_DEFAULT_MODE` env → `$XDG_CONFIG_HOME/ponytail/config.json`
   (Windows: `%APPDATA%\ponytail\config.json`) → `'full'`. The MCP server, pi
   extension, OpenCode plugin, and Hermes plugin all call into this module.

## 3. The ruleset (single source of truth)

`hooks/ponytail-instructions.js:73-88` is the canonical entry point:

```js
function getPonytailInstructions(mode) {
  const configuredMode = normalizePersistedMode(mode) || DEFAULT_MODE;
  if (INDEPENDENT_MODES.has(configuredMode)) {
    return 'PONYTAIL MODE ACTIVE — level: ' + configuredMode + '. Behavior defined by /ponytail-' + configuredMode + ' skill.';
  }
  const effectiveMode = normalizeMode(configuredMode) || DEFAULT_MODE;
  try {
    return 'PONYTAIL MODE ACTIVE — level: ' + effectiveMode + '\n\n' +
      filterSkillBodyForMode(fs.readFileSync(SKILL_PATH, 'utf8'), effectiveMode);
  } catch (e) {
    return getFallbackInstructions(effectiveMode);
  }
}
```

`SKILL.md` holds all mode-specific content in two structured forms:

- **Intensity table rows** matching `/^\|\s*\*\*(.+?)\*\*\s*\|/` → row label is
  a mode name (`lite`, `full`, `ultra`); keep only the row matching the active
  mode (`ponytail-instructions.js:21-26`).
- **Worked examples** matching `/^-\s*([^:]+):\s*/` → bullet label is a mode
  name; same filtering (`ponytail-instructions.js:28-32`).

Everything else — the ladder, the rules, the persistence banner, the
"when NOT to be lazy" carve-outs — is **mode-independent** and ships verbatim
on every level. The filter is line-level, regex-based, and intentionally
narrow: a bullet whose label is not a mode word (e.g. `- **No unrequested
abstractions:**`) is a normal rule and stays.

If the file cannot be read, `getFallbackInstructions(effectiveMode)`
(`ponytail-instructions.js:39-71`) returns a hand-written, complete copy of the
ruleset so the hook never emits a half-empty or empty context.

`INDEPENDENT_MODES = {'review'}` (`ponytail-instructions.js:8`) is a special
case: when the active mode is `review`, the ruleset is *not* injected — instead,
a one-liner points the agent at the `ponytail-review` skill, which carries its
own over-engineering-focused instructions. The runtime hook track accepts
`off` / `lite` / `full` / `ultra`; `review` is a config-file-only value that
flips the system into a review session.

## 4. Per-host integration

Each host differs in how often it can re-inject, where state lives, and what
shape its stdout contract takes. The plugin solves this with **one shared
builder + one shared config + one shared hook-runtime module** (`ponytail-runtime.js`)
that knows how to write each host's stdout envelope.

### 4.1 Claude Code (lifecycle hooks)

Manifest: `.claude-plugin/plugin.json:9` → `hooks/claude-codex-hooks.json`.

Three hook events, all using `${CLAUDE_PLUGIN_ROOT}`:

| Event | Matcher | Script | Purpose |
|---|---|---|---|
| `SessionStart` | `startup\|resume\|clear\|compact` | `ponytail-activate.js` | Write flag, emit ruleset, nudge statusline setup |
| `SubagentStart` | — | `ponytail-subagent.js` | Inject ruleset into Task-spawned subagents |
| `UserPromptSubmit` | — | `ponytail-mode-tracker.js` | Detect `/ponytail …` and write flag |

Three things make this work end to end:

1. **Flag file at `$CLAUDE_CONFIG_DIR/.ponytail-active`** (default `~/.claude/`).
   Written by `ponytail-activate.js` and `ponytail-mode-tracker.js`. Read by
   the statusline (`.sh` / `.ps1`) and by `ponytail-subagent.js`. CLAUDE_CONFIG_DIR
   is honored throughout, matching Claude Code's own convention (see comment
   in `ponytail-config.js:71-74`).
2. **Stdout shape per host** is centralized in `ponytail-runtime.js:33-59`:
   - Native Claude `SessionStart`: raw stdout = context string.
   - Native Claude `SubagentStart`: must use `hookSpecificOutput.additionalContext`
     (raw stdout is dropped here — comment at `ponytail-runtime.js:52`).
   - Codex: `{ systemMessage: "PONYTAIL:<MODE>", hookSpecificOutput: { … } }`.
   - Copilot: only `SessionStart` honors context, and only as `additionalContext`.
3. **Statusline nudge** (`ponytail-activate.js:45-85`): on first session, if
   `~/.claude/settings.json` has no `statusLine`, the hook appends a paragraph
   asking Claude to set up the badge. `isShellSafe(path)` gates whether the
   plugin can embed the command snippet — a clone path containing shell
   metacharacters (`; & $ ( ) …`) falls back to "wire it up manually." This
   is the only outbound side-effect of activation, and the host setting owns
   it.

Mode detection inside the session: `ponytail-mode-tracker.js` reads stdin JSON,
matches `/^[/@$]ponytail/`, and writes the requested mode to the flag. The
session-start activation runs `setMode(mode)` again on every fresh session so
the flag is reset to the configured default unless the user explicitly
overrode it (the flag lives outside the repo; the default-mode resolver is
called only by the activate hook on cold start, not by mode-tracker).

Deactivation phrase `isDeactivationCommand(text)` (`ponytail-config.js:40-43`)
matches only a whole-message `stop ponytail` or `normal mode` (lowercased,
trailing punctuation stripped). Mid-message matches are intentionally rejected
— issue noted inline: matching the phrase anywhere in the prompt turned ponytail
off for ordinary requests like "add a normal mode toggle."

### 4.2 Codex

Same lifecycle hooks (`hooks/claude-codex-hooks.json`), same scripts. Codex
distinguishes itself by:

- Setting `process.env.PLUGIN_DATA`, which `ponytail-runtime.js:6-12` reads to
  redirect the flag file there instead of `~/.claude`.
- Reading the session-start stdout as `{ systemMessage, hookSpecificOutput }`
  rather than raw text.
- Discovering commands through `commands/*.toml` (`commands/ponytail.toml`,
  `commands/ponytail-review.toml`, etc.) — these are prompt templates with
  `{{args}}` substitution, not the long-form skill bodies.
- Codex skill invocation is `@ponytail-review`, not `/ponytail-review`
  (`skills/ponytail-help/SKILL.md:33`).

### 4.3 OpenCode

Two integration files:

- `.opencode/plugins/ponytail.mjs` — server plugin. Three lifecycle hooks:
  - `config`: registers all six `.opencode/command/*.md` slash commands (parsed
    for `---\ndescription:\n…\n---\n<body>`, CRLF-tolerant) and adds the
    shared `skills/` directory to `config.skills.paths`.
  - `experimental.chat.system.transform`: reads the flag, appends
    `getPonytailInstructions(mode)` to `output.system` every turn. Returns
    early when mode is `off`.
  - `command.execute.before`: persists `/ponytail <level>` to a separate flag
    at `${XDG_CONFIG_HOME}/opencode/.ponytail-active`. Comment notes that
    this is async ("mode applies from the next message, not the current one;
    good enough; switch to a synchronous store if same-turn switching ever
    matters").

The OpenCode plugin bridges the CJS `hooks/ponytail-instructions.js` to ESM
via `createRequire(import.meta.url)`.

- `.opencode/command/*.md` — six frontmatter files. The plugin parses them
  itself; OpenCode does not have a TOML convention here.

### 4.4 pi agent

`pi-extension/index.js` is the extension entrypoint. It exposes:

- `parsePonytailCommand(text, defaultMode)` — handles `status`, `default <m>`,
  bare `lite|full|ultra|off`, with explicit invalid-result discriminated
  union (`{type, mode, reason}`).
- `resolveSessionMode(entries, fallbackMode)` — scans the session branch
  backwards for `customType: 'ponytail-mode'` entries written by
  `pi.appendEntry('ponytail-mode', { mode })`.
- `registerCommand` for `/ponytail`, `/ponytail-review`, `/ponytail-audit`,
  `/ponytail-gain`, `/ponytail-debt`, `/ponytail-help`. The five skill
  commands are aliases that send the skill name as a user message
  (`sendAlias`, lines 89-100); if the session is busy they are queued via
  `deliverAs: 'followUp'`.
- `pi.on('before_agent_start', …)` injects the ruleset into the agent's
  `systemPrompt`. This is the per-turn injection point.
- `pi.on('agent_start' | 'agent_end')` flip an `isActive` flag used by the
  status bar indicator.
- `syncStatus` writes a `ponytail` status-bar entry: icon
  (`🌿 / ⚡ / 🔥 / ''`) + label (`LITE / FULL / ULTRA`), with active state shown
  via theme `accent` (●) vs `dim` (○).

### 4.5 Hermes Agent

Pure-Python plugin loaded via `plugin.yaml` discovery. Entry: `register(ctx)`
(`__init__.py:195-217`). Three registrations:

- **Skills** — every `skills/<name>/SKILL.md` is registered by directory name;
  they become `ponytail:<skill>` skill references inside Hermes.
- **Hooks**:
  - `pre_llm_call` → `_pre_llm_call` (`__init__.py:125-128`): inject
    `build_injected_context(mode)` as `context` before every LLM turn. Mode
    is `_current_mode or _default_mode()`, where `_current_mode` is a
    module-local set by `/ponytail` and `_default_mode()` reads
    `PONYTAIL_DEFAULT_MODE` env → `~/.config/ponytail/config.json` (`defaultMode`)
    → `'full'`.
  - `pre_gateway_dispatch` → `rewrite_gateway_command` (`__init__.py:153-164`):
    intercepts `/ponytail-review`, `/ponytail-audit`, etc. (or `ponytail_review`
    with underscores normalized) and rewrites them into a normal agent prompt
    that reads `Load and follow the Hermes plugin skill ponytail:<command>`.
    Slash-command access is checked via `gateway._check_slash_access(source, command)`
    and rejected if denied (e.g. shared gateway, untrusted source).
- **Commands**:
  - `/ponytail [lite|full|ultra|off]` → `_handle_mode_command` sets the
    process-local `_current_mode` and reports back. Empty arg reports current.
  - Each skill command → `_make_skill_command_handler` calls
    `ctx.inject_message(prompt)`; success reports "Queued <cmd> for the agent.",
    failure returns the prompt string directly (degraded path).
- `build_injected_context(mode)` (`__init__.py:105-122`) reads
  `skills/ponytail/SKILL.md` and applies the **same mode-filter** algorithm as
  `hooks/ponytail-instructions.js` (a Python re-implementation:
  `__init__.py:70-87`). The "review" level loads `skills/ponytail-review/SKILL.md`
  instead. If the file can't be read, a hand-written `_fallback_instructions(mode)`
  is used (lines 90-102).

The Hermes plugin deliberately does **not** depend on the Node.js shared
hooks — it implements the same logic in Python so the plugin runs in-process
without spawning `node`. The two implementations are kept aligned by behavior
(not by code) — both must agree on the filter rules.

### 4.6 MCP server (`ponytail-mcp/`)

The cleanest single-purpose entry. Two surfaces, both delegating to the shared
builder:

- **Prompt `ponytail`**: returns the ruleset as a user message. User-invoked.
  Args: `{ mode?: 'lite' | 'full' | 'ultra' }`.
- **Tool `ponytail_instructions`**: same text plus `structuredContent:
  { mode, instructions }`. Annotations: `readOnlyHint: true`,
  `openWorldHint: false`. For hosts that pull context via tools / code exec.

`resolveMode()` (`instructions.js:16-22`) ignores `off`, unknown values, and
empty input — falls back to `getDefaultMode()` from the shared config, then
to `'full'`. So this server always returns rules; it never returns `''`.
The README explicitly calls out why this exists separately from the always-on
adapters: *"MCP prompts are user-invoked, and there is no portable MCP primitive
for 'inject this into every turn' across hosts. So this server is the clean
option for MCP hosts whose only injection point is the prompt menu, or that
pull context through tools. See issue #70."*

### 4.7 Instruction-only hosts (Cursor, Windsurf, Cline, Copilot editor, Kiro,
Antigravity, CodeWhale, Swival, VS Code + Codex extension)

These hosts have no hook / lifecycle API. The integration is **copy the
ruleset into the host's project-rules file** and check it stays aligned:

```
.cursor/rules/ponytail.mdc        (frontmatter stripped before compare)
.windsurf/rules/ponytail.md       (raw text)
.clinerules/ponytail.md           (raw text)
.agents/rules/ponytail.md         (raw text)
.github/copilot-instructions.md   (raw text)
.kiro/steering/ponytail.md        (frontmatter stripped before compare)
```

`scripts/check-rule-copies.js` walks this list, normalizes each, byte-compares
to the canonical body of `AGENTS.md`, then runs an invariant canary over both
`AGENTS.md` and `skills/ponytail/SKILL.md` (8 load-bearing phrases — see §11).

The Gemini CLI extension manifest `gemini-extension.json:5` sets
`contextFileName: "AGENTS.md"`, which tells Gemini to read it as always-on
context. The hooks map is intentionally **not** placed at Gemini's
auto-discovered `hooks/hooks.json` path — Gemini's auto-loader expects
Claude/Codex event names anyway.

## 5. Slash commands

| Command | Skill | What it does |
|---|---|---|
| `/ponytail [lite\|full\|ultra\|off]` | `ponytail` | Set the active intensity. No arg → report current. |
| `/ponytail-review` | `ponytail-review` | Diff-only over-engineering review. Output: `L<line>: <tag> <what>. <replacement>.` Tags: `delete:`, `stdlib:`, `native:`, `yagni:`, `shrink:`. End with `net: -<N> lines possible.` |
| `/ponytail-audit` | `ponytail-audit` | Whole-repo audit, same tag vocabulary, ranked biggest cut first. |
| `/ponytail-debt` | `ponytail-debt` | Greps `ponytail:` comments → ledger. Flags `no-trigger` rows. |
| `/ponytail-gain` | `ponytail-gain` | One-shot benchmark scoreboard. No persistence, no mode change. |
| `/ponytail-help` | `ponytail-help` | One-shot reference card. No persistence, no mode change. |

Two skills explicitly refuse to mutate state: `ponytail-gain` and `ponytail-help`
are read-only displays. `ponytail-debt` reads the repo and reports; it can be
asked to write the ledger to `PONYTAIL-DEBT.md` but does not do so by default.
`ponytail-review` and `ponytail-audit` are explicitly out of scope for
correctness, security, and performance — they only hunt complexity.

## 6. Mode persistence

| Host | Storage | Read by | Written by |
|---|---|---|---|
| Claude Code | `$CLAUDE_CONFIG_DIR/.ponytail-active` | statusline, `ponytail-subagent.js` | `ponytail-activate.js`, `ponytail-mode-tracker.js` |
| Codex | `${PLUGIN_DATA}/.ponytail-active` | same | same |
| Copilot CLI | `${COPILOT_PLUGIN_DATA}/.ponytail-active` | same | same |
| OpenCode | `${XDG_CONFIG_HOME}opencode/.ponytail-active` | `experimental.chat.system.transform` | `command.execute.before` |
| pi agent | session branch entry `customType: 'ponytail-mode'` | `resolveSessionMode` | `pi.appendEntry('ponytail-mode', …)` on every mode set |
| Hermes Agent | process-local `_current_mode`; defaults via env/file | `_pre_llm_call` | `/ponytail` handler |
| Generic / MCP | resolved at request time from env/file | `ponytail-config.js` | `writeDefaultMode` (called by `ponytail default <m>` in pi) |

The flag-file model is **single-line text**: one mode word per file. No JSON
wrapper, no concurrent writer protection — a single host at a time, and
failures are silent (every hook is wrapped in `try { … } catch (e) {}`).
The comment on `ponytail-runtime.js:23-24` documents this contract:
*"Live mode written by activate/mode-tracker. Absent flag = ponytail off."*

Default resolution order is documented in `ponytail-config.js:1-10` and is
identical across hosts that use it: env > config file > `'full'`.

## 7. The statusline

Two scripts (`hooks/ponytail-statusline.sh` and `.ps1`) read the flag from
`CLAUDE_CONFIG_DIR` (default `~/.claude`). Both produce `\033[38;5;108m` (sage)
output. For `full` (the default) they print `[PONYTAIL]`; otherwise
`[PONYTAIL:<MODE>]`. The flag's first non-whitespace line is the mode;
empty or missing flag → exit 0 silently.

The statusline is **opt-in**: `ponytail-activate.js` only nudges setup if
`statusLine` is not already configured. The nudge emits a snippet only when
the install path passes `isShellSafe` (`ponytail-config.js:50-52`) — the
allowlist is `[A-Za-z0-9 _.\-:/\\~]`, sufficient for normal Windows/POSIX
paths, fails closed on quotes, `&`, `$`, backtick, `;`, etc.

## 8. Lifecycle of a session

```
┌─────────────────────────────────────────────────────────────────────┐
│ SessionStart (cold)                                                 │
│   ponytail-activate.js:                                             │
│     1. mode = getDefaultMode()        [env > file > 'full']         │
│     2. if mode == 'off': clear flag, write empty, exit              │
│     3. setMode(mode) → write flag                                   │
│     4. context = getPonytailInstructions(mode)  [filtered SKILL.md] │
│     5. if statusLine missing && host is native Claude:              │
│          append nudge to context (gated by isShellSafe)             │
│     6. writeHookOutput(SessionStart, mode, context)                 │
│        → shape by isCodex / isCopilot / native                      │
└─────────────────────────────────────────────────────────────────────┘
                              ↓
┌─────────────────────────────────────────────────────────────────────┐
│ Each user prompt (warm)                                             │
│   ponytail-mode-tracker.js (UserPromptSubmit):                      │
│     stdin JSON → parse prompt                                       │
│     if /^[/@$]ponytail/:                                            │
│       parse arg → mode (lite|full|ultra|off|review)                 │
│       setMode or clearMode                                          │
│       writeHookOutput(UserPromptSubmit, mode, 'MODE CHANGED…')      │
│     if isDeactivationCommand(prompt): clear flag, emit 'MODE OFF'   │
└─────────────────────────────────────────────────────────────────────┘
                              ↓
┌─────────────────────────────────────────────────────────────────────┐
│ Each LLM turn                                                       │
│   Claude: prepended via SessionStart context (no per-turn hook)     │
│   Subagent: ponytail-subagent.js reads flag, re-injects             │
│   OpenCode: experimental.chat.system.transform appends every turn   │
│   pi: before_agent_start injects into systemPrompt every turn       │
│   Hermes: pre_llm_call injects context every turn                   │
│   MCP: user must invoke prompt/tool; no per-turn injection          │
└─────────────────────────────────────────────────────────────────────┘
```

The asymmetric design here is deliberate. Claude's lifecycle can't re-inject
mid-session cheaply, so `SessionStart` carries the context for the whole
session and `SubagentStart` re-injects for spawned agents (issue #252:
without this, Task-spawned agents ran ponytail-unaware). OpenCode and pi
have per-turn hooks, so they append fresh. Hermes has `pre_llm_call` so
it injects fresh too.

## 9. Build, test, publish

- `npm test` (root) → `node --test tests/*.test.js && npm test --prefix pi-extension`.
  Ten test files: behavior, commands, copilot-plugin, correctness,
  gemini-extension, hermes-plugin, hooks (with windows sibling), openclaw-skills,
  opencode-plugin, uninstall.
- `node scripts/check-rule-copies.js` — invariant lint across 6 copied rule
  files + 8 phrase canaries (see §11). Fails the suite if any drift.
- `node scripts/check-versions.js` — version-pinning lint across manifests
  (`package.json`, `plugin.json`, `__init__.py` if present, etc.).
- `node scripts/build-openclaw-skills.js` — generates `.openclaw/skills/`
  from `skills/`. The `tests/openclaw-skills.test.js` suite fails if generated
  output is stale.
- `node scripts/publish-openclaw-skills.js` — publishes all six skills to
  ClawHub at the `package.json` version. `--dry-run` for preview.
- `node scripts/uninstall.js` — removes state outside the plugin: the flag
  file at `$CLAUDE_CONFIG_DIR/.ponytail-active`, the config file at the
  platform-specific path, and the `statusLine` entry in `settings.json` if
  its command string contains `ponytail-statusline` (substring match; a
  combined statusline gets removed wholesale — see `ponytail:` comment on
  `scripts/uninstall.js:31-33`).

## 10. Tests as documentation

The test files are short and readable; they document the plugin's contract
more precisely than the README does:

- `tests/hooks.test.js` — exercises `isShellSafe`, the `CLAUDE_CONFIG_DIR` /
  `PLUGIN_DATA` / `COPILOT_PLUGIN_DATA` env-var branches in
  `ponytail-runtime.js`, and the per-host stdout envelope produced by
  `writeHookOutput`. Comment at lines 28-33 explicitly notes that the base
  env must be scrubbed of `PLUGIN_DATA` / `COPILOT_PLUGIN_DATA` or the
  native-Claude assertions mis-fire.
- `tests/commands.test.js`, `tests/behavior.test.js` — slash-command
  resolution and mode-override behavior across hosts.
- `tests/hermes-plugin.test.js` — the Python side: `_filter_skill_body_for_mode`,
  `build_injected_context`, `parsePonytailCommand`-style logic, and the
  `/ponytail-*` command rewriting.
- `tests/opencode-plugin.test.js` — the `.mjs` plugin's three hooks.
- `tests/copilot-plugin.test.js`, `tests/gemini-extension.test.js` — host
  manifest wiring.
- `tests/openclaw-skills.test.js` — `node scripts/build-openclaw-skills.js`
  output is current.
- `tests/uninstall.test.js` — the cleanup script only removes what ponytail
  wrote (doesn't touch a user-set `statusLine`).

## 11. Invariants the project enforces

`scripts/check-rule-copies.js:43-56` pins eight substrings that must survive
verbatim in both `skills/ponytail/SKILL.md` and `AGENTS.md`. These are the
load-bearing rules — reword them and the check fails, which is the reminder
to propagate the change to the six copied rule files:

| Invariant | Why it's pinned |
|---|---|
| `naive heuristic` | The `ponytail:` ceiling-comment contract. |
| `ONE runnable check` | The non-trivial-logic test reflex. |
| `flimsier algorithm` | Same-size stdlib → pick the edge-case-correct one. |
| `input validation at trust boundaries` | Safety carve-out #1. |
| `prevents data loss` | Safety carve-out #2 (wraps a line in SKILL.md). |
| `security` | Safety carve-out #3. |
| `accessibility` | Safety carve-out #4. |
| `Lazy code without its check is unfinished` | Test reflex promoted to headline. |

These are also the four "not lazy about" carve-outs from
`skills/ponytail/SKILL.md:88-92` and `AGENTS.md:30`. Pinning each substring
prevents a reword in either file from silently dropping a safety guarantee
from a copied rule.

## 12. Failure modes & design tradeoffs (honest)

- **No concurrent write protection on the flag file.** Single-host assumption.
  Two Claude sessions on the same machine racing on the flag will last-write-win.
  Acceptable: the flag is one word, the worst case is a stale mode for one
  prompt.
- **The SKILL.md filter is line-level regex.** A `ponytail:` *comment* in a
  code block is fine; a `- **full**` list item that is not a mode label will
  still be inspected by the table-label regex and skipped if `full` ≠ active
  mode. The code disambiguates by checking `normalizeMode(label)` first; only
  recognized mode names drop the line. Random prose bullets are safe.
- **Hermes plugin is Python, the rest is Node.** Drift between the two
  `filterSkillBodyForMode` implementations is caught only at runtime by the
  Hermes test. There's no generator: a future contributor who changes one
  must change the other.
- **MCP server is the only host that doesn't always-on-inject.** This is
  stated as a limitation in the README and treated as a feature for hosts
  whose only injection point is a prompt menu. If a host gains a portable
  "always inject this user prompt" primitive, the MCP server becomes
  redundant (issue #70).
- **The OpenCode plugin's mode change applies on the next turn.** The comment
  in `.opencode/plugins/ponytail.mjs:88-91` calls this out. The transform
  reads the flag synchronously, but the write is async; same-turn switching
  is not supported. The author rates this "good enough."
- **Statusline nudge is one-shot per session.** Once the user accepts or
  rejects the suggestion, the nudge never repeats — it gates on the presence
  of a `statusLine` entry, not on its content.
- **The two benchmark numbers disagree.** The README shows both `−54%` agentic
  (12 tickets on a real repo, n=4, Haiku 4.5) and `−80-94%` single-shot. The
  single-shot number was criticized in issue #126 as a conversational-baseline
  artifact, and the README now flags it explicitly. The agentic number is the
  defensible one; the single-shot number is preserved for reproduction only.

## 13. How to integrate it into a new host

The pattern, in order:

1. **Locate the host's always-on context slot.** Most agents have one
   (system prompt, instructions file, project rules). For hosts that don't,
   the MCP server is the clean fallback.
2. **Resolve the active mode.** Call `getDefaultMode()` from
   `hooks/ponytail-config.js` (or re-implement in the host's native language,
   matching the env > file > `'full'` order and the
   `$XDG_CONFIG_HOME` / `%APPDATA%` / `~/.config` platform branches).
3. **Build the context.** Call `getPonytailInstructions(mode)` and emit the
   string verbatim. If the host has lifecycle hooks, write the flag and
   append the string to system prompt on each turn. If not, copy
   `skills/ponytail/SKILL.md` (or the compact `AGENTS.md`) into the
   host's rule file.
4. **Wire `/ponytail <level>`.** Write the mode to whatever state store the
   host has; on next turn the resolver picks it up.
5. **Skip the MCP path** if the host already has lifecycle hooks — the MCP
   server is for prompt-menu-only hosts.
6. **Pin invariants.** If you copy the rule text, run
   `scripts/check-rule-copies.js` (or port its eight substrings) in CI so
   any drift fails the build.

That's the integration surface: a 94-line instruction builder, a 122-line
config resolver, a 68-line runtime that knows three stdout shapes, and six
host adapters. Everything else is documentation and tests.

## 14. Files touched in this report

- Read in full: `AGENTS.md`, `README.md`, `package.json`, `plugin.yaml`,
  `__init__.py`, `opencode.json`, `gemini-extension.json`,
  `.claude-plugin/{marketplace,plugin}.json`, `hooks/{ponytail-config,
  ponytail-instructions, ponytail-runtime, ponytail-activate,
  ponytail-mode-tracker, ponytail-subagent, ponytail-statusline.sh,
  ponytail-statusline.ps1, claude-codex-hooks.json, copilot-hooks.json}.js`,
  `.opencode/plugins/ponytail.mjs`, `.opencode/command/{ponytail,
  ponytail-review}.md`, `pi-extension/{index.js, package.json}`,
  `ponytail-mcp/{index.js, instructions.js, package.json, README.md}`,
  `skills/ponytail/{SKILL.md, review, audit, debt, gain, help}/SKILL.md`,
  `commands/{ponytail, ponytail-review}.toml`, `scripts/{uninstall.js,
  check-rule-copies.js}`, `docs/{agent-portability.md, platform-native.md}`,
  `after-install.md`.
- Read in part: `tests/hooks.test.js` (first 50 lines — enough for the
  test-harness conventions), `pi-extension/test/` (file list only).
