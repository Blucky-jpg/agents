# Run-Until-Paper Plan — Long-Running Research Loop on Raspberry Pi 5

**Date**: 2026-06-26
**Target**: `/home/blucky/Agents/co-scientist/`
**Predecessor**: `IMPROVEMENT_PLAN.md` (Tier 1 + Tier 2 shipped, 7 of 8 items green)
**Goal**: Make the supervisor loop until a research-grade output is produced, with crash-resume so the Pi 5 can run unattended for days.

---

## §0 Executive summary

The current `Supervisor::run` (supervisor.rs:74) drives a **fixed-shape pipeline**: 3 generation tasks → 3 reflection tasks → 1 ranking batch → evolution if mature → metareview every 50 matches → terminate on Elo stability or budget. It runs in ~3-4 minutes for the inspected workload and writes a final report.

Three problems block "run until paper" on a Pi 5:

1. **No experiment loop.** The 14 prompt templates include `reflection_verification`, `reflection_observation`, `evolution_feasibility` — all designed for testing hypotheses — but `decide_next_steps` (supervisor.rs:299) never enqueues them. Hypotheses are reviewed, ranked, evolved, and meta-reviewed but never verified against evidence.
2. **Termination is shallow.** `check_termination` (supervisor.rs:387) knows exactly three reasons: `deadline`, `budget_usd`, `elo_stability`. None corresponds to "we have a strong, experimentally-supported insight." The `elo_stability` arm fires as soon as top-5 Elos don't change for 3 cycles in a row — usually within minutes of the first tournament batch.
3. **No resume.** If the process dies (power blip, OOM, panic recovery), all in-flight tasks are abandoned. `tasks` rows stay `pending`/`cancelled`; nothing auto-restarts.

This plan addresses (1) and (3) directly. (2) gets a single safety-net arm (`max_no_progress_cycles`) and one configurable Elo-with-experiments check, both off by default to preserve current behavior.

**Estimated code surface**: ~250 LOC across `supervisor.rs`, `run_agent.rs`, `main.rs`, plus one new `prompts/runner.rs` integration test and one new markdown file. No schema changes. No new external dependencies.

---

## §1 Goals

- **A.** Wire experiment verification into the supervisor pipeline so top hypotheses actually get tested, not just ranked.
- **D.** Add crash-resume: a `cargo run -- resume <session_id>` subcommand that re-attaches to a half-completed session and continues from the last unclaimed task.
- **C-light.** Replace the `elo_stability` exit with a safer "no-progress" check that gives the experiment loop time to produce results before quitting.

Out of scope for this plan (deliberately):
- **B** (periodic re-generation when the field "moves"): evolution already does a weaker version. Defer until A is validated.
- **C-full** ("strong insight" Elo+experiment threshold): research target, not termination criteria. Defer.
- **§2.3 council fan-out** (IMPROVEMENT_PLAN): unrelated, skipped per user.

---

## §2 Phase A — Experiment loop

### §2.1 Current state

`prompts.rs` already has these agent modes (file:line):

- `reflection_observation` (prompts/reflection_observation.md) — design an experiment to test a hypothesis
- `reflection_verification` (prompts/reflection_verification.md) — verify or falsify a hypothesis from observations
- `evolution_feasibility` (prompts/evolution_feasibility.md) — assess whether an evolved hypothesis is testable

`RunAgentTool` (run_agent.rs:74-201) handles agent dispatch. `enqueue_follow_ups` (run_agent.rs:506-595) currently enqueues:
- `generation` → reflection per hypothesis
- `reflection` → ranking
- `evolution` → reflection per evolved hypothesis

No path produces `reflection_observation` or `reflection_verification` tasks. Nothing in `decide_next_steps` enqueues experiment tasks either.

### §2.2 Where to wire it

Extend `decide_next_steps` (supervisor.rs:299) with a third trigger after the evolution block:

```
For each hypothesis with elo ≥ maturity_threshold AND matches_played ≥ 3:
  if it has no associated verification semantic_memory:
    enqueue reflection_verification task scoped to that hypothesis_id
```

Two new `SupervisorConfig` fields:

```rust
pub verification_elo_floor: f64,     // default 1300.0 (was unused Elo)
pub verification_min_matches: usize, // default 3
```

### §2.3 Follow-up wiring

Extend `enqueue_follow_ups` `reflection` branch (run_agent.rs:544-562) to also enqueue a `reflection_observation` task when the review concludes the hypothesis is testable. The existing 5-Whys + error/cause/fix schema on `save_behavior` (§1.4 of IMPROVEMENT_PLAN) means the LLM can record experiment plans in the same format.

### §2.4 Verification result → Elo

When `reflection_verification` saves a `result` or `experiment` semantic memory (scope `result` or `experiment`), bump the linked hypothesis's Elo by a small amount (e.g. +16 for confirm, −16 for falsify, +0 for inconclusive). This makes the Elo score reflect experimental backing, not just peer review strength.

Implementation: add `record_verification(hypothesis_id, outcome)` method to `HypothesisRepo` (hypothesis.rs), call from `RunAgentTool::call` after the marker dispatch for a verification task. ~30 LOC.

### §2.5 Verification result → research_sessions.final_report

When `finalize` (supervisor.rs:441) runs, include the verification summaries in the final report. The current report only cites hypotheses and tournament rationales. Adding experimental evidence is what makes it a paper-grade output rather than a research framing.

### §2.6 Files

- `src/supervisor.rs` — extend `decide_next_steps`, add 2 config fields
- `src/run_agent.rs` — extend `enqueue_follow_ups` `reflection` branch, add verification dispatch
- `src/hypothesis.rs` — add `record_verification(hypothesis_id, outcome)` method
- `src/supervisor.rs:441` (`finalize`) — include verification summaries in final report
- `tests/integration.rs` — new test: enqueue verification task after Elo+match threshold, assert task runs and verification semantic memory inserted

### §2.7 Estimated effort

~3 hours.

---

## §3 Phase D — Resume

### §3.1 Current state

`main.rs:50` dispatches to `cmd_start` for the `start` subcommand. `cmd_start` (main.rs:237) creates a fresh `Supervisor` and runs it. There is no `resume` subcommand.

`tasks` table (queue.rs) uses lease-based claims with reclaim (queue.rs:191-232, queue.rs:383-419). Pending tasks stay pending if no worker claims them. Cancelled tasks (set by `finalize` at supervisor.rs:441) stay cancelled forever.

### §3.2 What resume does

`cargo run -- resume <session_id>` (or `cargo run -- continue <session_id>`):

1. Open the existing DB at `$CO_SCIENTIST_DB`
2. Look up `research_sessions.id == <session_id>`
3. If `status == 'done'`: error "session already finalized; use `inspect` instead"
4. Reset all `cancelled` tasks for this session back to `pending` (those were cancelled during the previous run's `finalize`)
5. Reset any `running` tasks (orphaned by process death) to `pending` after their lease expires
6. Spawn the supervisor with the existing `session_id`, `goal`, `preferences`, `started_at`, and any in-progress hypothesis state intact
7. Run the supervisor's main loop until the new termination criteria (§4) hit

### §3.3 Required helpers

`HypothesisRepo::load_session_state(session_id) -> Option<SupervisorState>` — read the session row + hypothesis counts + match counts to rebuild the `Supervisor` struct. The current `Supervisor::new` takes these from CLI args; `resume` reads them from the DB.

Add to `supervisor.rs` constructor:
```rust
pub async fn resume(memory: Memory, queue: TaskQueue, registry: ..., prompts: ..., session_id: &str) -> Result<Self>
```

### §3.4 Tasks table reaping

The reaping step (reset cancelled → pending, wait for expired leases) needs a new function in `queue.rs:reap_stale_for_session(session_id)`. Reuses the existing `reclaim_expired` (queue.rs:383-419) for the lease-expiry half; adds the cancelled-reset half.

### §3.5 CLI subcommand

Add to `main.rs`:

```rust
"resume" | "continue" => cmd_resume(&args, &db_path).await?,
```

`cmd_resume` accepts `<session_id>` as first arg, optional `--config <path>` for non-default `SupervisorConfig`.

### §3.6 Systemd unit for Pi 5

Add a sample unit file at `contrib/co-scientist.service` that:
- Sets `WorkingDirectory=/home/pi/Agents`
- Sets `Environment=CO_SCIENTIST_DB=/home/pi/.co_scientist.db`
- Restarts on crash with 30s backoff
- Logs to journald
- User: `pi`

This is a documentation/delivery artifact, not Rust code.

### §3.7 Files

- `src/main.rs` — add `cmd_resume`, wire subcommand
- `src/supervisor.rs` — add `Supervisor::resume` constructor
- `src/queue.rs` — add `reap_stale_for_session(session_id)`
- `src/hypothesis.rs` — add `HypothesisRepo::load_session_state`
- `contrib/co-scientist.service` — new file
- `tests/integration.rs` — new test: start a session, kill it mid-run (close DB), resume with same session_id, verify tasks get re-enqueued

### §3.8 Estimated effort

~2 hours.

---

## §4 Phase C-light — Termination safety net

### §4.1 Current termination (supervisor.rs:387-438)

Three reasons, evaluated in order:
1. `deadline` — `Duration` after start time (default 0 = disabled)
2. `budget_usd` — heuristic from `turn_completed` event payload sizes (default 0 = disabled)
3. `elo_stability` — top-5 Elos unchanged for `stability_threshold` (default 3) consecutive checks (default ε=25.0)

Default config has none of these set: `deadline=ZERO`, `budget_usd=0.0`, so the only effective exit is `elo_stability` after the first ranking batch. The inspected run completed in ~4 minutes because `elo_stability` fired on the first check — none of the hypotheses had played matches yet, so all 5 Elos were 1200 and the snapshot was "stable."

### §4.2 New termination arm: `max_no_progress_cycles`

One new field:

```rust
pub max_no_progress_cycles: usize, // default 8 (≈8 supervisor idle-injection rounds with no change)
```

In `check_termination`, after the existing `elo_stability` arm:

```rust
// No-progress safety net: N consecutive idle-injection rounds with
// neither Elo changes nor new experiments. Prevents infinite runs on
// dead goals (e.g. a research question that produces 3 hypotheses but
// none can be experimentally tested).
let progressed = last_round_changed_elo || last_round_added_experiment;
if !progressed {
    no_progress_counter += 1;
    if no_progress_counter >= self.config.max_no_progress_cycles {
        return Ok(Some("no_progress".to_string()));
    }
} else {
    no_progress_counter = 0;
}
```

State to track:
- `last_round_changed_elo: bool` — set in `decide_next_steps` based on top-5 snapshot diff
- `last_round_added_experiment: bool` — set in `decide_next_steps` based on whether any verification task was enqueued
- `no_progress_counter: usize` — supervisor struct field

### §4.3 Make existing arms explicit

Add `deadline: Option<Duration>`-style optionality in docs even though the type stays `Duration` (use `is_zero()`). Same for `budget_usd: f64` (`> 0.0` = enabled).

### §4.4 Files

- `src/supervisor.rs` — extend `check_termination`, add `no_progress_counter` to struct, set the two `last_round_*` flags in `decide_next_steps`, add 1 config field

### §4.5 Estimated effort

~1 hour.

---

## §5 Recommended execution order

```
1. §4.1 C-light   1h    unblocks long runs on its own; safe to ship independently
2. §2.1 A         3h    core capability — without this the system can't write a paper
3. §3.1 D         2h    operational requirement for unattended Pi 5 use
4. (ship, observe on Pi 5, decide whether to do §1.B and §1.C-full)
```

Total: ~6 hours of focused work. All three phases can be implemented as separate PRs.

---

## §6 Verification plan

### §6.1 Phase A — experiment loop

1. **Build**: `cargo check --features test-helpers -p co-scientist` succeeds.
2. **Tests**: existing 104 tests still pass; new integration test `verification_task_fires_after_elo_threshold` passes.
3. **Behavioral check**: run the inspected goal "topologie of neural nets" again; expect at least one `reflection_verification` task to be enqueued within the first tournament batch and a `result` semantic memory to appear in the final report.

### §6.2 Phase D — resume

1. **Build + tests**: same as §6.1.
2. **Behavioral check**: run a session, kill the process with SIGKILL during a turn. Run `cargo run -- resume <session_id>`. Verify the pending and cancelled tasks get re-enqueued and the run continues. Verify the final report still gets written.

### §6.3 Phase C-light — no-progress exit

1. **Build + tests**: same as §6.1.
2. **Behavioral check**: run with a goal the model can't make progress on. Expect the run to exit with reason `no_progress` after the configured number of idle rounds.

---

## §7 Configuration preview

After all three phases ship, a typical Pi 5 deployment `SupervisorConfig` would look like:

```rust
SupervisorConfig {
    budget_usd: 0.0,           // disabled; we're on a Pi, not API-metered
    deadline: Duration::from_secs(7 * 24 * 3600), // 1 week hard cap
    concurrency: 2,            // Pi 5 has 4 cores; leave 2 for OS / consolidation
    stability_threshold: 5,    // was 3; longer stability window
    stability_epsilon: 25.0,
    min_hypotheses: 3,
    min_mature: 5,
    meta_review_interval: 20,  // was 50; smaller intervals for long runs
    n_initial: 3,
    initial_elo: 1200.0,
    // New from Phase A:
    verification_elo_floor: 1300.0,
    verification_min_matches: 3,
    // New from Phase C-light:
    max_no_progress_cycles: 8,
}
```

---

## §8 Out of scope / explicit non-goals

- **Real experiment execution** (running actual ML training, lab automation, web scraping). The verification prompt asks the LLM to *design* and *interpret* experiments; co-scientist doesn't run them. A future plan can add a "compute tool" that the verification agent calls to actually execute the experiment.
- **Multi-session learning**. Each session is independent. A future plan can have sessions cite each other as prior work via `prior_session_summary`.
- **LLM cost accounting**. The budget heuristic is rough; real metering needs the API provider's actual token counts. Out of scope.
- **Web UI / TUI**. The CLI `cargo run -- inspect <session_id>` is enough for now; a TUI was explicitly deferred in prior sessions.

---

## §9 Source files referenced

- `src/supervisor.rs` (532 lines) — main loop, termination, idle injection
- `src/run_agent.rs` (596 lines) — agent dispatch, follow-up enqueueing
- `src/hypothesis.rs` (310 lines) — hypothesis repo
- `src/queue.rs` (667 lines) — durable task queue, lease management
- `src/main.rs` (515 lines) — CLI dispatch
- `prompts/reflection_observation.md`, `prompts/reflection_verification.md`, `prompts/evolution_feasibility.md` — verification prompt templates (already exist, unused)
- `IMPROVEMENT_PLAN.md` — predecessor plan, all §1 + §2 (minus §2.3) shipped