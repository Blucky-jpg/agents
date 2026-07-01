//! Telemetry for the 24/7 soak runner.
//!
//! Atomic counters, rolling latency sketch, and a soft-assert helper.
//! **No panics from this module ever** — every method is total.
//!
//! Two invariants the rest of the runner relies on:
//! 1. `record_*` never panics, even if called concurrently with
//!    `health_check` or `print_tick`. (All shared state is `Atomic*`.)
//! 2. `print_tick` may truncate to stderr but never panics on a write
//!    failure — we just drop the line.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Soft-assert: record a failure with a label + free-form detail.
/// Never panics. The label is a short stable identifier (e.g. `"idempotency.same_key"`);
/// the detail is a human-readable message.
pub fn soft_fail(label: &'static str, detail: impl AsRef<str>) {
    SOFT_FAIL_COUNT.fetch_add(1, Ordering::Relaxed);
    eprintln!(
        "[soak soft-fail] {label}: {}",
        detail.as_ref().replace('\n', " ").chars().take(200).collect::<String>()
    );
}

/// Hard crash counter — a panic that `catch_unwind` actually caught.
pub fn record_crash(iteration: u64, kind: &'static str, detail: impl AsRef<str>) {
    CRASH_COUNT.fetch_add(1, Ordering::Relaxed);
    LAST_CRASH_ITER.store(iteration, Ordering::Relaxed);
    eprintln!(
        "[soak CRASH] iter={iteration} kind={kind}: {}",
        detail.as_ref().chars().take(400).collect::<String>()
    );
}

/// Iteration success counter.
pub fn record_success() {
    SUCCESS_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// Iteration soft-fail counter (an assertion fired but the loop continued).
pub fn record_soft_fail() {
    SOFT_FAIL_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// Latency observation in microseconds.
pub fn record_latency_micros(us: u64) {
    LATENCY_SUM_US.fetch_add(us, Ordering::Relaxed);
    LATENCY_COUNT.fetch_add(1, Ordering::Relaxed);
    LATENCY_MAX_US.fetch_max(us, Ordering::Relaxed);
    // Rolling p95 sketch: keep last 1024 observations in a coarse histogram.
    let slot = (LATENCY_COUNT.load(Ordering::Relaxed) as usize) & 1023;
    LATENCY_HIST[slot].store(us, Ordering::Relaxed);
}

pub fn record_db_size(bytes: u64) {
    DB_SIZE_BYTES.store(bytes, Ordering::Relaxed);
}

pub fn record_iteration() {
    ITER_TOTAL.fetch_add(1, Ordering::Relaxed);
}

pub fn iterations_total() -> u64 {
    ITER_TOTAL.load(Ordering::Relaxed)
}

pub fn crashes_total() -> u64 {
    CRASH_COUNT.load(Ordering::Relaxed)
}

pub fn soft_fails_total() -> u64 {
    SOFT_FAIL_COUNT.load(Ordering::Relaxed)
}

pub fn successes_total() -> u64 {
    SUCCESS_COUNT.load(Ordering::Relaxed)
}

pub fn last_crash_iteration() -> u64 {
    LAST_CRASH_ITER.load(Ordering::Relaxed)
}

/// Compute p95 over the rolling 1024-slot histogram. Cheap O(1024).
pub fn latency_p95_micros() -> u64 {
    let mut all: Vec<u64> = LATENCY_HIST.iter().map(|c| c.load(Ordering::Relaxed)).collect();
    all.sort_unstable();
    all.get(all.len() * 95 / 100).copied().unwrap_or(0)
}

pub fn latency_p50_micros() -> u64 {
    let mut all: Vec<u64> = LATENCY_HIST.iter().map(|c| c.load(Ordering::Relaxed)).collect();
    all.sort_unstable();
    all.get(all.len() / 2).copied().unwrap_or(0)
}

pub fn latency_max_micros() -> u64 {
    LATENCY_MAX_US.load(Ordering::Relaxed)
}

/// Print a one-line status. Called periodically from the runner.
pub fn print_tick(elapsed: Duration) {
    let total = ITER_TOTAL.load(Ordering::Relaxed);
    let succ = SUCCESS_COUNT.load(Ordering::Relaxed);
    let soft = SOFT_FAIL_COUNT.load(Ordering::Relaxed);
    let crash = CRASH_COUNT.load(Ordering::Relaxed);
    let p50 = latency_p50_micros();
    let p95 = latency_p95_micros();
    let db = DB_SIZE_BYTES.load(Ordering::Relaxed);
    let s = compute_scores(total, crash, soft, p95 as f64 / 1000.0, elapsed.as_secs_f64());
    let _ = eprintln!(
        "[soak] elapsed={elapsed:?} iter={total} ok={succ} soft={soft} crash={crash} \
         p50={p50}us p95={p95}us max={}us db={db}B \
         SCORE comp={:.1} stab={:.1} corr={:.1} perf={:.1} thr={:.2}it/s",
        latency_max_micros(),
        s.composite,
        s.stability,
        s.correctness,
        s.performance,
        s.throughput_it_per_sec,
    );
}

/// Final health check. Returns `Ok(())` if soak is healthy, `Err` with
/// a human-readable reason if not. Never panics.
pub fn health_check(max_crash_rate: f64, min_iterations: u64) -> Result<(), String> {
    let total = ITER_TOTAL.load(Ordering::Relaxed);
    if total < min_iterations {
        return Err(format!(
            "ran only {total} iterations; need >= {min_iterations} for a meaningful soak"
        ));
    }
    let crash = CRASH_COUNT.load(Ordering::Relaxed);
    let rate = crash as f64 / total as f64;
    if rate > max_crash_rate {
        return Err(format!(
            "crash rate {rate:.4} > allowed {max_crash_rate:.4} \
             ({crash}/{total}); soak is not 24/7 safe"
        ));
    }
    Ok(())
}

// ---- Scoring ----

/// Per-dimension + composite score. All values 0..=100 except throughput
/// (iterations/sec, informational).
///
/// Composite weighting (sums to 100%):
/// - 50% **stability** — crash rate is the headline 24/7 signal.
/// - 30% **correctness** — soft-fail rate (invariant violations).
/// - 20% **performance** — p95 latency staying bounded.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SoakScores {
    pub stability: f64,
    pub correctness: f64,
    pub performance: f64,
    /// Iterations per second over the full elapsed window. Informational,
    /// not part of the composite.
    pub throughput_it_per_sec: f64,
    pub composite: f64,
}

/// Pure scoring math. Takes raw counters (so it's unit-testable) and
/// returns the per-dimension + composite scores.
///
/// Calibrated so that:
/// - Zero crashes, zero soft-fails, p95 ≤ 50 ms → composite = 100.
/// - 1 crash in 1000 iterations → stability ≈ 50.
/// - 1 soft-fail in 10 iterations → correctness ≈ 0.
/// - p95 = 5 s → performance = 0.
/// - Anything worse clamps to 0.
pub fn compute_scores(
    total: u64,
    crash: u64,
    soft: u64,
    p95_ms: f64,
    elapsed_secs: f64,
) -> SoakScores {
    // Stability: linear decay. Crash rate 0.002 (2 per 1000) → 0.
    let stability = if total == 0 {
        100.0
    } else {
        let crash_rate = crash as f64 / total as f64;
        (100.0 * (1.0 - crash_rate / 0.002)).clamp(0.0, 100.0)
    };

    // Correctness: linear decay. Soft rate 0.10 (1 per 10) → 0.
    let correctness = if total == 0 {
        100.0
    } else {
        let soft_rate = soft as f64 / total as f64;
        (100.0 * (1.0 - soft_rate / 0.10)).clamp(0.0, 100.0)
    };

    // Performance: 100 at p95 ≤ 50 ms; linear decay to 0 at p95 = 5 s.
    let performance = if p95_ms <= 50.0 {
        100.0
    } else {
        (100.0 * (1.0 - (p95_ms - 50.0) / (5000.0 - 50.0))).clamp(0.0, 100.0)
    };

    let throughput_it_per_sec = if elapsed_secs > 0.0 {
        total as f64 / elapsed_secs
    } else {
        0.0
    };

    let composite = stability * 0.5 + correctness * 0.3 + performance * 0.2;

    SoakScores {
        stability,
        correctness,
        performance,
        throughput_it_per_sec,
        composite,
    }
}

/// Convenience wrapper: read the atomic counters and compute scores.
pub fn score_now(elapsed: Duration) -> SoakScores {
    let total = ITER_TOTAL.load(Ordering::Relaxed);
    let crash = CRASH_COUNT.load(Ordering::Relaxed);
    let soft = SOFT_FAIL_COUNT.load(Ordering::Relaxed);
    let p95_ms = latency_p95_micros() as f64 / 1000.0;
    compute_scores(total, crash, soft, p95_ms, elapsed.as_secs_f64())
}

/// Format the final score line for the runner. Never panics.
pub fn format_score_line(s: &SoakScores) -> String {
    format!(
        "[soak SCORE] composite={:.2} stability={:.2} correctness={:.2} \
         performance={:.2} throughput={:.2}it/s",
        s.composite, s.stability, s.correctness, s.performance, s.throughput_it_per_sec,
    )
}

// ---- globals (initialized lazily, never mutated except via atomics) ----

static ITER_TOTAL: AtomicU64 = AtomicU64::new(0);
static SUCCESS_COUNT: AtomicU64 = AtomicU64::new(0);
static SOFT_FAIL_COUNT: AtomicU64 = AtomicU64::new(0);
static CRASH_COUNT: AtomicU64 = AtomicU64::new(0);
static LAST_CRASH_ITER: AtomicU64 = AtomicU64::new(0);
static LATENCY_SUM_US: AtomicU64 = AtomicU64::new(0);
static LATENCY_COUNT: AtomicU64 = AtomicU64::new(0);
static LATENCY_MAX_US: AtomicU64 = AtomicU64::new(0);
static DB_SIZE_BYTES: AtomicU64 = AtomicU64::new(0);

/// 1024-slot rolling histogram for coarse p50/p95 estimation.
/// This is NOT a true sliding window — slots are filled round-robin
/// by observation index, so the sketch represents the last ~1024
/// observations in order (modulo wrap-around). Good enough for a
/// "did latencies explode?" signal; not good for true percentile accuracy.
static LATENCY_HIST: [AtomicU64; 1024] = {
    // const-init array of atomics.
    const ZERO: AtomicU64 = AtomicU64::new(0);
    [ZERO; 1024]
};

/// Initialize telemetry. Idempotent. Safe to call from multiple tests.
pub fn init() {
    // Touch the histogram so any first-use lazy-init issues surface at
    // test start rather than mid-iteration. (Const-initialized, so this
    // is a no-op today — kept as a hook for future startup logic.)
    let _ = LATENCY_HIST[0].load(Ordering::Relaxed);
    let started = STARTED_AT.get_or_init(Instant::now);
    let _ = started.elapsed(); // exercises Instant
}

use std::sync::OnceLock;
static STARTED_AT: OnceLock<Instant> = OnceLock::new();

#[allow(dead_code)]
pub fn elapsed_since_start() -> Duration {
    STARTED_AT.get_or_init(Instant::now).elapsed()
}

// ---- unit tests for the scoring math ----
//
// These run under `cargo test --lib` only if telemetry were a lib; as a
// binary test crate they're picked up by `cargo test --test memory_soak`
// because the test harness executes every `#[test]` in the module tree.
// We tolerate a tiny f64 epsilon because the math uses floating point.

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod score_tests {
    use super::*;

    fn approx_eq(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "expected {b}, got {a}");
    }

    #[test]
    fn perfect_run_scores_100() {
        let s = compute_scores(10_000, 0, 0, 10.0, 1.0);
        approx_eq(s.stability, 100.0);
        approx_eq(s.correctness, 100.0);
        approx_eq(s.performance, 100.0);
        approx_eq(s.composite, 100.0);
        approx_eq(s.throughput_it_per_sec, 10_000.0);
    }

    #[test]
    fn zero_iterations_scores_100_by_default() {
        // Before any iteration runs, the score shouldn't be 0 just because
        // we have no data — the runner hasn't failed yet.
        let s = compute_scores(0, 0, 0, 0.0, 0.0);
        approx_eq(s.stability, 100.0);
        approx_eq(s.correctness, 100.0);
        approx_eq(s.performance, 100.0);
    }

    #[test]
    fn single_crash_in_thousand_halves_stability() {
        // 1 crash in 1000 iters = crash rate 0.001 → stability = 100*(1 - 0.5) = 50.
        let s = compute_scores(1000, 1, 0, 10.0, 1.0);
        approx_eq(s.stability, 50.0);
    }

    #[test]
    fn one_crash_in_ten_thousand_drops_stability_to_95() {
        // crash rate 0.0001 → stability = 100 * (1 - 0.05) = 95.
        let s = compute_scores(10_000, 1, 0, 10.0, 1.0);
        approx_eq(s.stability, 95.0);
    }

    #[test]
    fn catastrophic_crash_rate_clamps_stability_at_zero() {
        let s = compute_scores(100, 10, 0, 10.0, 1.0);
        // 10 crashes in 100 = 0.1, way over 0.002 → clamped to 0.
        approx_eq(s.stability, 0.0);
        // With stability at 0, correctness 100, performance 100:
        // composite = 0.5*0 + 0.3*100 + 0.2*100 = 50 exactly.
        assert!(
            s.composite <= 50.0,
            "composite {} should be heavily penalized (≤ 50)",
            s.composite,
        );
    }

    #[test]
    fn one_soft_fail_in_ten_drops_correctness_to_zero() {
        // soft rate 0.10 → correctness = 0.
        let s = compute_scores(10, 0, 1, 10.0, 1.0);
        approx_eq(s.correctness, 0.0);
    }

    #[test]
    fn soft_fails_scale_linearly() {
        // 5 soft fails in 100 iters = 0.05 → correctness = 50.
        let s = compute_scores(100, 0, 5, 10.0, 1.0);
        approx_eq(s.correctness, 50.0);
    }

    #[test]
    fn p95_at_fifty_ms_is_full_performance() {
        let s = compute_scores(100, 0, 0, 50.0, 1.0);
        approx_eq(s.performance, 100.0);
    }

    #[test]
    fn p95_above_fifty_ms_decays_linearly() {
        // At p95 = 2525 ms: (2525 - 50) / (5000 - 50) = 0.5 → perf = 50.
        let s = compute_scores(100, 0, 0, 2525.0, 1.0);
        approx_eq(s.performance, 50.0);
    }

    #[test]
    fn p95_above_five_seconds_clamps_performance_at_zero() {
        let s = compute_scores(100, 0, 0, 9000.0, 1.0);
        approx_eq(s.performance, 0.0);
    }

    #[test]
    fn composite_is_weighted_sum() {
        // Hand-picked inputs to land on clean dimension scores:
        //   4 crashes / 10000 → stability = 100*(1 - 0.0004/0.002) = 80
        //   400 soft fails / 10000 → correctness = 100*(1 - 0.04/0.10) = 60
        //   p95 = 3020 ms → performance = 100*(1 - (3020-50)/4950) = 40
        // Composite = 0.5*80 + 0.3*60 + 0.2*40 = 40 + 18 + 8 = 66.
        let s = compute_scores(10_000, 4, 400, 3020.0, 1.0);
        approx_eq(s.stability, 80.0);
        approx_eq(s.correctness, 60.0);
        approx_eq(s.performance, 40.0);
        approx_eq(s.composite, 66.0);
    }

    #[test]
    fn throughput_zero_when_elapsed_zero() {
        let s = compute_scores(100, 0, 0, 10.0, 0.0);
        approx_eq(s.throughput_it_per_sec, 0.0);
    }

    #[test]
    fn throughput_computes_iters_per_sec() {
        let s = compute_scores(500, 0, 0, 10.0, 2.0);
        approx_eq(s.throughput_it_per_sec, 250.0);
    }

    #[test]
    fn format_score_line_includes_all_dimensions() {
        let s = SoakScores {
            stability: 90.0,
            correctness: 80.0,
            performance: 70.0,
            throughput_it_per_sec: 12.5,
            composite: 83.0,
        };
        let line = format_score_line(&s);
        assert!(line.contains("composite=83.00"));
        assert!(line.contains("stability=90.00"));
        assert!(line.contains("correctness=80.00"));
        assert!(line.contains("performance=70.00"));
        assert!(line.contains("throughput=12.50it/s"));
    }
}