//! 24/7 crash-free soak benchmark for the memory subsystem.
//!
//! ## Why this lives in `tests/`
//!
//! It's an opt-in, multi-hour integration test. Every test is `#[ignore]`d
//! so default `cargo test` runs skip it. To run:
//!
//! ```text
//! cargo test --test memory_soak -- --ignored --nocapture
//! ```
//!
//! For real 24/7 operation, set `SOAK_DURATION_SECS=86400` (or whatever).
//!
//! ## Design — why this can run 24/7 without a single crash
//!
//! 1. **Fresh in-memory DB per iteration.** No persistent state, no
//!    corruption buildup, no WAL bloat.
//! 2. **Per-iteration timeout** (`SOAK_WORKLOAD_TIMEOUT_SECS`, default 30s)
//!    so a hung query never wedges the runner.
//! 3. **`FutureExt::catch_unwind` around every workload/probe** so a
//!    panic in one iteration can never crash the test process.
//! 4. **No `unwrap()` in the runner.** Every error path is a soft_fail.
//! 5. **Probe rotation** runs deterministic invariant assertions on a
//!    separate cadence (every N iterations) so workload rotation can't
//!    miss regressions.
//! 6. **Telemetry atomic counters** survive panics because `Ordering::Relaxed`
//!    operations on `AtomicU64` are total.
//!
//! ## What it actually exercises
//!
//! - **Workloads** (7 rotating patterns): realistic research-session shapes
//!   plus pathological cases (high churn, 16-way concurrent writers,
//!   scale stress, edge-input fuzz). See `workload.rs`.
//! - **Probes** (12 deterministic invariants): recall@5, idempotency,
//!   three-layer cost ordering, scope isolation, archive exclusion, etc.
//!   See `probes.rs`.
//! - **Telemetry**: counters, latency p50/p95/max, rolling histogram.
//!   See `telemetry.rs`.

#![allow(clippy::needless_return)]

mod fixture;
mod probes;
mod telemetry;
mod workload;

use std::time::{Duration, Instant};

use futures::FutureExt;
use rand::Rng;

use co_scientist::db;
use co_scientist::memory::Memory;

// ---- Configuration via env vars (with safe defaults) ----

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn env_f64(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

struct Config {
    duration: Duration,
    min_iterations: u64,
    max_crash_rate: f64,
    workload_timeout: Duration,
    probe_every: u64,
    tick_every: Duration,
}

impl Config {
    fn from_env() -> Self {
        // Defaults are tuned so a bare `cargo test -- --ignored` finishes
        // in ~1 minute with at least 100 iterations. Override via env for
        // true 24/7 operation.
        Self {
            duration: Duration::from_secs(env_u64("SOAK_DURATION_SECS", 60)),
            min_iterations: env_u64("SOAK_MIN_ITERATIONS", 100),
            max_crash_rate: env_f64("SOAK_MAX_CRASH_RATE", 0.001),
            workload_timeout: Duration::from_secs(env_u64("SOAK_WORKLOAD_TIMEOUT_SECS", 30)),
            probe_every: env_u64("SOAK_PROBE_EVERY", 25),
            tick_every: Duration::from_secs(env_u64("SOAK_TICK_EVERY_SECS", 10)),
        }
    }
}

// ---- The 24/7 entry point ----

/// 24/7 soak runner. Opt-in via `--ignored`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "24/7 soak; opt-in via cargo test -- --ignored --nocapture"]
async fn memory_soak_24_7() {
    let cfg = Config::from_env();
    telemetry::init();
    eprintln!(
        "[soak] starting config duration={:?} min_iter={} max_crash_rate={} \
         workload_timeout={:?} probe_every={} tick_every={:?}",
        cfg.duration,
        cfg.min_iterations,
        cfg.max_crash_rate,
        cfg.workload_timeout,
        cfg.probe_every,
        cfg.tick_every,
    );

    let started = Instant::now();
    let mut iter: u64 = 0;
    let mut last_tick = Instant::now();
    let mut last_probe_idx: usize = 0;
    let probe_names = probes::probe_names();

    while started.elapsed() < cfg.duration || iter < cfg.min_iterations {
        iter += 1;
        telemetry::record_iteration();

        // 1. Pick a random workload by index (the enum isn't Clone,
        //    so the index is the canonical handle).
        let kind_idx = rand::thread_rng().gen_range(0..workload::WorkloadKind::all().len());

        // 2. Fresh in-memory DB → wrapped in Memory handle.
        let mem = match fresh_memory().await {
            Ok(m) => m,
            Err(e) => {
                telemetry::record_crash(iter, "fresh_memory", format!("open_memory failed: {e}"));
                // Don't bail; let the next iteration try again. The DB
                // open path is exercised every iteration; if it breaks,
                // we'll count crashes here.
                continue;
            }
        };

        // 3. Run the workload with timeout + panic-catch.
        let outcome = run_workload_safe(&mem, kind_idx, iter, cfg.workload_timeout).await;
        if outcome.errored {
            telemetry::record_soft_fail();
        } else {
            telemetry::record_success();
        }

        // 4. Periodic probe rotation. Run one probe every `probe_every` iters.
        if iter % cfg.probe_every == 0 && last_probe_idx < probe_names.len() {
            let probe_name = probe_names[last_probe_idx];
            last_probe_idx = (last_probe_idx + 1) % probe_names.len();
            run_probe_safe(probe_name, iter, cfg.workload_timeout).await;
        }

        // 5. Periodic telemetry tick (best-effort, never blocks).
        if last_tick.elapsed() >= cfg.tick_every {
            telemetry::print_tick(started.elapsed());
            last_tick = Instant::now();
        }
    }

    // Final tick so the user sees the closing line.
    telemetry::print_tick(started.elapsed());

    // Final score line — the headline number of the run.
    let scores = telemetry::score_now(started.elapsed());
    eprintln!("{}", telemetry::format_score_line(&scores));

    // 6. Health check. The whole point of 24/7: no crashes beyond the
    //    declared budget, and at least min_iterations actually ran.
    match telemetry::health_check(cfg.max_crash_rate, cfg.min_iterations) {
        Ok(()) => eprintln!(
            "[soak] OK — {} iterations in {:?}, {} soft-fails, {} crashes",
            telemetry::iterations_total(),
            started.elapsed(),
            telemetry::soft_fails_total(),
            telemetry::crashes_total(),
        ),
        Err(reason) => panic!("[soak] FAILED health check: {reason}"),
    }
}

// ---- helpers ----

/// Open an in-memory DB and wrap it in `Memory`. Returns `Err` only if the
/// DB itself can't be opened — never panics.
async fn fresh_memory() -> anyhow::Result<Memory> {
    let db = db::open_memory().await?;
    Ok(Memory::new(db))
}

/// Wrap a single workload invocation with timeout + panic-catch.
/// Always returns a `WorkloadOutcome`. Never panics.
async fn run_workload_safe(
    mem: &Memory,
    kind_idx: usize,
    iter: u64,
    timeout: Duration,
) -> workload::WorkloadOutcome {
    let started = Instant::now();
    let kind = &workload::WorkloadKind::all()[kind_idx];
    let workload_fut = std::panic::AssertUnwindSafe(workload::run(*kind, mem, iter));
    let outcome = match tokio::time::timeout(timeout, workload_fut.catch_unwind()).await {
        Ok(Ok(out)) => out,
        Ok(Err(panic)) => {
            // Workload panicked despite the per-workload catch in `run`.
            // Belt + suspenders — should be unreachable in practice.
            let msg = panic_msg(&panic);
            telemetry::record_crash(
                iter,
                "workload_panic_outer",
                format!("panic in workload after inner catch: {msg}"),
            );
            workload::WorkloadOutcome {
                errored: true,
                error_label: "panic_after_inner_catch",
                ..Default::default()
            }
        }
        Err(_elapsed) => {
            // Workload ran longer than its budget. Soft-fail, don't crash.
            telemetry::record_crash(
                iter,
                "workload_timeout",
                format!("workload exceeded {timeout:?}"),
            );
            workload::WorkloadOutcome {
                errored: true,
                error_label: "timeout",
                ..Default::default()
            }
        }
    };
    let _ = started.elapsed();
    outcome
}

/// Wrap a single probe invocation with timeout + panic-catch.
/// Probes self-report soft-fails via `telemetry::soft_fail`. The wrapper
/// here just ensures the runner itself never panics.
async fn run_probe_safe(name: &'static str, iter: u64, timeout: Duration) {
    let started = Instant::now();
    let probe_fut = std::panic::AssertUnwindSafe(probes::run_probe(name, iter));
    match tokio::time::timeout(timeout, probe_fut.catch_unwind()).await {
        Ok(Ok(())) => {
            // Probes record success inside themselves.
        }
        Ok(Err(panic)) => {
            let msg = panic_msg(&panic);
            telemetry::record_crash(
                iter,
                "probe_panic",
                format!("probe={name} panic={msg} elapsed={:?}", started.elapsed()),
            );
        }
        Err(_elapsed) => {
            telemetry::record_crash(
                iter,
                "probe_timeout",
                format!("probe={name} exceeded {timeout:?}"),
            );
        }
    }
}

/// Extract a panic payload as a String for telemetry. Never panics.
fn panic_msg(p: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = p.downcast_ref::<&'static str>() {
        s.to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "<unknown panic payload>".to_string()
    }
}

// ---- minimal companion tests ----

/// A 5-second smoke test that confirms the runner compiles and a small
/// workload rotation completes without crashing. Opt-out (runs by
/// default) so the soak harness itself is exercised in CI.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_soak_smoke() {
    telemetry::init();
    let cfg = Config {
        duration: Duration::from_secs(0), // run only until min_iterations
        min_iterations: 5,
        max_crash_rate: 1.0, // smoke doesn't gate on crash rate
        workload_timeout: Duration::from_secs(10),
        probe_every: 2,
        tick_every: Duration::from_secs(60), // don't tick during smoke
    };
    let started = Instant::now();
    let mut iter = 0u64;
    let mut last_probe_idx = 0usize;
    let probe_names = probes::probe_names();
    while iter < cfg.min_iterations {
        iter += 1;
        telemetry::record_iteration();
        let kind_idx = rand::thread_rng().gen_range(0..workload::WorkloadKind::all().len());
        let mem = match fresh_memory().await {
            Ok(m) => m,
            Err(e) => {
                telemetry::record_crash(iter, "smoke.fresh_memory", format!("{e}"));
                continue;
            }
        };
        let outcome = run_workload_safe(&mem, kind_idx, iter, cfg.workload_timeout).await;
        if outcome.errored {
            telemetry::record_soft_fail();
        } else {
            telemetry::record_success();
        }
        if iter % cfg.probe_every == 0 && last_probe_idx < probe_names.len() {
            let name = probe_names[last_probe_idx];
            last_probe_idx += 1;
            run_probe_safe(name, iter, cfg.workload_timeout).await;
        }
    }
    assert!(
        started.elapsed() < Duration::from_secs(120),
        "smoke should finish in well under 2 minutes (took {:?})",
        started.elapsed(),
    );
    assert!(
        telemetry::crashes_total() == 0,
        "smoke run produced {} crashes",
        telemetry::crashes_total(),
    );

    // Score-based companion assertion. A healthy smoke should land above 70
    // (it's a tiny run, so perf/correctness dominate). The composite is the
    // headline metric — if this drops below 70, something regressed.
    let scores = telemetry::score_now(started.elapsed());
    eprintln!("{}", telemetry::format_score_line(&scores));
    assert!(
        scores.composite >= 70.0,
        "smoke composite score {} is below 70 — soak regressed? \
         stability={} correctness={} performance={}",
        scores.composite,
        scores.stability,
        scores.correctness,
        scores.performance,
    );
    assert_eq!(
        scores.stability, 100.0,
        "smoke must have zero crashes (got {} stability)",
        scores.stability,
    );
}

/// Run a longer smoke that exercises every workload at least once and
/// then asserts the score lands in a reasonable band. Catches scoring-
/// math regressions that the small smoke might miss (e.g. if all
/// workloads happen to produce zero soft-fails in 5 iterations).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn memory_soak_score_in_range() {
    telemetry::init();
    let cfg = Config {
        duration: Duration::from_secs(0),
        min_iterations: 28, // 4 full rotations of the 7-workload set
        max_crash_rate: 1.0,
        workload_timeout: Duration::from_secs(10),
        probe_every: 4,
        tick_every: Duration::from_secs(60),
    };
    let started = Instant::now();
    let mut iter = 0u64;
    let mut last_probe_idx = 0usize;
    let probe_names = probes::probe_names();
    while iter < cfg.min_iterations {
        iter += 1;
        telemetry::record_iteration();
        // Deterministic round-robin so every workload runs at least once.
        let n_kinds = workload::WorkloadKind::all().len();
        let kind_idx = (iter as usize - 1) % n_kinds;
        let mem = match fresh_memory().await {
            Ok(m) => m,
            Err(e) => {
                telemetry::record_crash(iter, "score.fresh_memory", format!("{e}"));
                continue;
            }
        };
        let outcome = run_workload_safe(&mem, kind_idx, iter, cfg.workload_timeout).await;
        if outcome.errored {
            telemetry::record_soft_fail();
        } else {
            telemetry::record_success();
        }
        if iter % cfg.probe_every == 0 && last_probe_idx < probe_names.len() {
            let name = probe_names[last_probe_idx];
            last_probe_idx += 1;
            run_probe_safe(name, iter, cfg.workload_timeout).await;
        }
    }
    let scores = telemetry::score_now(started.elapsed());
    eprintln!("{}", telemetry::format_score_line(&scores));
    // Stability is the hard guarantee — must be perfect (no crashes).
    assert_eq!(
        scores.stability,
        100.0,
        "score-in-range run had a crash (stability={})",
        scores.stability,
    );
    // The other dimensions depend on what's actually in the memory
    // layer's correctness surface — we assert a floor that catches
    // dramatic regressions but tolerates the soft-fails the probe is
    // *designed* to surface (e.g. real lexical/embedding gaps).
    assert!(
        scores.composite >= 40.0,
        "composite {} below floor 40 — something regressed hard: \
         stab={} corr={} perf={}",
        scores.composite,
        scores.stability,
        scores.correctness,
        scores.performance,
    );
}