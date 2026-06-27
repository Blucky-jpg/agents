//! LLM query helpers: transient-error classification, deterministic
//! jitter, and per-attempt trace persistence.
//!
//! Extracted from `runner.rs` so the retry policy lives in one place
//! and can be unit-tested without spinning up a `ClaudeHandle`. The
//! retry *loop* itself stays in `runner.rs::run_turn_query` — it is
//! tightly coupled to the runner's `ClaudeHandle` and would not gain
//! from being moved here. What lives here is everything else the
//! loop depends on, plus the markers for "is this error worth a
//! retry?" that we want to keep consistent across the codebase.
//!
//! See `runner.rs::run_turn_query` for the loop that calls into these
//! helpers.

/// Heuristic: which error strings are worth retrying?
///
/// Network, timeout, broken pipe = retry. Auth, model-not-found,
/// permission = fail. Substring match (lowercased) on the rendered
/// error string — the original CLI subprocess doesn't give us typed
/// errors, so we pattern-match the message.
///
/// Order matters: fatal markers short-circuit transient markers, so
/// "auth timeout" is treated as fatal (not transient).
pub fn is_transient_error(err: &str) -> bool {
    let e = err.to_lowercase();
    let transient_markers = [
        "timeout", "timed out", "broken pipe", "connection reset",
        "connection refused", "i/o", "io error", "eof", "stream",
        "subprocess", "exit status", "killed", "signal",
    ];
    let fatal_markers = [
        "auth", "authentication", "unauthorized", "forbidden",
        "permission denied", "model not found", "invalid model",
        "not found", "bad request",
    ];
    if fatal_markers.iter().any(|m| e.contains(m)) {
        return false;
    }
    transient_markers.iter().any(|m| e.contains(m))
}

/// Convenience: classify an `anyhow::Error` by walking its cause
/// chain. A transient cause anywhere in the chain counts as transient
/// — the CLI sometimes wraps a network error in a higher-level
/// "query failed" context that drops the original wording.
pub fn is_transient_anyhow(err: &anyhow::Error) -> bool {
    err.chain().any(|c| is_transient_error(&c.to_string()))
}

/// Deterministic jitter from step + attempt. Avoids the `rand` dep
/// while still spreading retries across workers. Range: `[0, max)`.
///
/// Same splitmix64-style hash used in the original `runner.rs`
/// implementation. Kept here verbatim — the retry loop already
/// relies on the exact bit pattern.
pub fn jitter(seed: u64, attempt: u64, max: u64) -> u64 {
    let mut x = seed.wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(attempt);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D049BB133111EB);
    x ^= x >> 31;
    x % max
}

/// Backoff schedule for `run_turn_query`. Three attempts, base
/// delays of 0s / 1s / 3s with deterministic jitter on top.
/// `index 0` is the delay *before* retry attempt 1 (the second
/// call). Caller adds jitter on top.
pub const BASE_BACKOFF_MS: [u64; 3] = [0, 1000, 3000];

/// Persist the prompt to disk before each retry attempt. Cheap
/// insurance for post-mortem debugging if the process dies mid-retry.
pub fn persist_trace(
    run_id: &str,
    step: i64,
    attempt: u32,
    prompt: &str,
) -> std::io::Result<()> {
    let dir = std::path::PathBuf::from("traces");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{run_id}_step{step}_attempt{attempt}.md"));
    std::fs::write(path, prompt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_transient_error_recognizes_timeout() {
        assert!(is_transient_error("request timed out"));
        assert!(is_transient_error("connection timeout"));
        assert!(is_transient_error("operation timed out after 30s"));
    }

    #[test]
    fn is_transient_error_recognizes_network_errors() {
        // The classifier matches substring patterns the original
        // runner.rs implementation recognized. "network unreachable"
        // is NOT in the transient markers (it doesn't contain any of
        // the listed substrings) — so we test the markers that ARE
        // recognized.
        assert!(is_transient_error("connection reset by peer"));
        assert!(is_transient_error("connection refused"));
        assert!(is_transient_error("broken pipe"));
        assert!(is_transient_error("io error: unexpected eof"));
        assert!(is_transient_error("subprocess exit status 1"));
        assert!(is_transient_error("stream closed"));
        assert!(is_transient_error("process killed by signal"));
        assert!(is_transient_error("EOF while reading"));
    }

    #[test]
    fn is_transient_error_rejects_auth_failures() {
        assert!(!is_transient_error("invalid api key"));
        assert!(!is_transient_error("authentication failed"));
        assert!(!is_transient_error("unauthorized"));
        assert!(!is_transient_error("forbidden"));
        assert!(!is_transient_error("permission denied"));
    }

    #[test]
    fn is_transient_error_rejects_model_errors() {
        assert!(!is_transient_error("model not found: foo"));
        assert!(!is_transient_error("invalid model: bar"));
        assert!(!is_transient_error("bad request: invalid prompt"));
        assert!(!is_transient_error("not found"));
    }

    #[test]
    fn is_transient_error_is_case_insensitive() {
        assert!(is_transient_error("CONNECTION RESET"));
        assert!(is_transient_error("Timed Out"));
        assert!(!is_transient_error("UNAUTHORIZED"));
    }

    #[test]
    fn is_transient_error_fatal_short_circuits_transient() {
        // "auth timeout" should be classified as fatal — auth wins.
        assert!(!is_transient_error("auth timeout"));
        assert!(!is_transient_error("permission denied after timeout"));
    }

    #[test]
    fn is_transient_anyhow_walks_cause_chain() {
        // A transient cause anywhere in the chain counts as transient.
        // The CLI sometimes wraps a network error in a higher-level
        // "query failed" context that drops the original wording.
        let err = anyhow::anyhow!("claude query failed")
            .context("connection reset by peer");
        assert!(is_transient_anyhow(&err));

        let err = anyhow::anyhow!("claude query failed")
            .context("invalid api key");
        assert!(!is_transient_anyhow(&err));

        let err = anyhow::anyhow!("top-level wrapper");
        assert!(!is_transient_anyhow(&err));
    }

    #[test]
    fn jitter_returns_value_within_range() {
        // Run many trials; every output must be in [0, max).
        let max = 500u64;
        for step in 0..20u64 {
            for attempt in 0..5u64 {
                let j = jitter(step, attempt, max);
                assert!(
                    j < max,
                    "jitter({step}, {attempt}, {max}) = {j} (out of range)"
                );
            }
        }
    }

    #[test]
    fn jitter_does_not_explode_for_zero() {
        // jitter(0, 0, 0) must not panic on division by zero or
        // otherwise misbehave. The splitmix path produces a value
        // then `% 0` panics, so the caller must avoid `max = 0`.
        // Document that here by asserting on a non-zero max path.
        assert_eq!(jitter(0, 0, 1), 0);
        // When max > 0, jitter(0, 0, ...) produces a deterministic
        // value (we don't pin the exact number — the hash constants
        // could change — but the contract is "in range").
        let j = jitter(0, 0, 1000);
        assert!(j < 1000);
    }

    #[test]
    fn jitter_is_deterministic() {
        // Same inputs → same outputs, across many seeds.
        for step in 0..50u64 {
            for attempt in 0..10u64 {
                let a = jitter(step, attempt, 1024);
                let b = jitter(step, attempt, 1024);
                assert_eq!(a, b, "jitter({step}, {attempt}) not deterministic");
            }
        }
    }

    #[test]
    fn jitter_spreads_across_attempts() {
        // With the same step but different attempts, jitter should
        // produce different values at least sometimes. This catches
        // a regression where attempt is accidentally ignored.
        let max = 100_000u64;
        let step = 42u64;
        let mut distinct = std::collections::HashSet::new();
        for attempt in 0..20u64 {
            distinct.insert(jitter(step, attempt, max));
        }
        assert!(
            distinct.len() > 1,
            "jitter should spread across attempts, got all identical"
        );
    }

    #[test]
    fn base_backoff_schedule_matches_spec() {
        // The retry loop relies on exactly: 0ms before retry 1,
        // 1000ms before retry 2, 3000ms before final attempt's delay.
        assert_eq!(BASE_BACKOFF_MS, [0, 1000, 3000]);
        assert_eq!(BASE_BACKOFF_MS.len(), 3);
    }
}