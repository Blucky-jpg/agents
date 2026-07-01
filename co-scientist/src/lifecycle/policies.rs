//! Pure decision policies used by the Supervisor orchestrator.
//!
//! Extracted from `supervisor.rs` so the decision logic can be unit-tested
//! without spinning up the full async / DB stack. The orchestrator feeds
//! in counts and counters and gets a `Decision` back.
//!
//! These are pure functions of their inputs — no `async`, no `Db`,
//! no `Memory` handle. The Supervisor owns one of each policy as a
//! field and calls them inside its hot loop.

use std::time::Duration;

/// What the idle-injection policy decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InjectDecision {
    /// Queue is drained and the grace window has elapsed — try to spawn
    /// more work. The Supervisor's `decide_next_steps` then chooses
    /// which agents to enqueue.
    SpawnReflectionAgent,
    /// Don't inject yet — either there's still work in flight, or the
    /// grace window hasn't elapsed, or the initial pipeline hasn't
    /// produced anything yet.
    NoAction,
}

/// Counter snapshot the Supervisor already tracks. Held by value so the
/// policy is testable without lifetimes into `Supervisor`.
#[derive(Debug, Clone, Copy, Default)]
pub struct RunCounters {
    pub tasks_completed: usize,
}

/// Idle-injection policy: pure struct holding the grace window. No DB,
/// no async. The Supervisor passes in the live counters and gets a
/// decision.
#[derive(Debug, Clone, Copy)]
pub struct IdlePolicy {
    /// Don't fire idle injection unless `since_last >= grace`. Prevents
    /// a race where the last completion and the next worker claim
    /// overlap.
    pub grace: Duration,
}

impl Default for IdlePolicy {
    fn default() -> Self {
        Self {
            grace: Duration::from_secs(10),
        }
    }
}

impl IdlePolicy {
    /// Decide whether the idle-injection path should run. Mirrors the
    /// supervisor.rs:217-226 inline predicate verbatim:
    ///   pending == 0 && inflight == 0 && tasks_completed > 0
    ///     && since_last >= grace
    pub fn should_inject(
        &self,
        counters: RunCounters,
        pending: usize,
        inflight: usize,
        since_last: Duration,
    ) -> InjectDecision {
        if pending == 0
            && inflight == 0
            && counters.tasks_completed > 0
            && since_last >= self.grace
        {
            InjectDecision::SpawnReflectionAgent
        } else {
            InjectDecision::NoAction
        }
    }
}

/// Snapshot of run state fed to [`TerminationPolicy::evaluate`]. Held by
/// reference because it bundles the (potentially growing) stability
/// snapshot vector.
#[derive(Debug, Clone)]
pub struct RunSnapshot {
    pub elapsed: Duration,
    pub deadline: Duration,
    pub budget_usd: f64,
    pub budget_spent_usd: f64,
    pub top_hypotheses: Vec<(i64, f64)>,
    pub min_hypotheses: usize,
    pub stability_epsilon: f64,
    pub stability_threshold: usize,
    /// Number of historical snapshots already pushed (not including the
    /// current one). Matches the supervisor's
    /// `stability_snapshots.len()` before the push.
    pub snapshot_count: usize,
    /// The most recent prior snapshot, if any. Used to detect stability.
    pub previous_snapshot: Option<Vec<(i64, f64)>>,
    /// Total tournament matches recorded so far in this session. Used
    /// to gate the elo_stability termination: zero matches means Elo
    /// scores haven't been tested through comparison, so "stable" is
    /// trivially true and would terminate a fresh session before the
    /// tournament has even started.
    pub match_count: usize,
}

/// Termination verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminationDecision {
    Continue,
    Terminate { reason: String },
}

/// Pure termination policy. Mirrors the three branches of
/// `supervisor.rs::check_termination`: deadline, budget, Elo stability.
/// The Elo branch mutates the `previous_snapshot` into `Some(current)` —
/// to keep this function pure, we instead take the previous snapshot
/// and the supervisor pushes the new one back after the call returns.
#[derive(Debug, Clone, Copy, Default)]
pub struct TerminationPolicy;

impl TerminationPolicy {
    pub fn new() -> Self {
        Self
    }

    /// Decide whether to terminate. Returns `Terminate { reason }` if
    /// any of (deadline, budget, Elo stability) trigger, else
    /// `Continue`. The reason string matches the strings the inline
    /// code emitted: `"deadline"`, `"budget (...)"`, `"elo_stability"`.
    pub fn evaluate(&self, snap: &RunSnapshot) -> TerminationDecision {
        // Deadline.
        if !snap.deadline.is_zero() && snap.elapsed >= snap.deadline {
            return TerminationDecision::Terminate {
                reason: "deadline".to_string(),
            };
        }

        // Budget.
        if snap.budget_usd > 0.0 && snap.budget_spent_usd >= snap.budget_usd {
            return TerminationDecision::Terminate {
                reason: format!("budget ({:.2} USD)", snap.budget_spent_usd),
            };
        }

        // Elo stability.
        if snap.top_hypotheses.len() >= snap.min_hypotheses {
            let current: Vec<(i64, f64)> = snap.top_hypotheses.clone();
            let stable = snap
                .previous_snapshot
                .as_ref()
                .map(|prev| {
                    current.len() == prev.len()
                        && current.iter().zip(prev.iter()).all(
                            |((id_a, ea), (id_b, eb))| {
                                id_a == id_b && (ea - eb).abs() < snap.stability_epsilon
                            },
                        )
                })
                .unwrap_or(false);
            // Stability only counts once we have at least
            // `stability_threshold` snapshots in history. Inline code
            // checked `snapshots.len() >= threshold` AFTER pushing the
            // current one, so termination fires on the iteration that
            // brings the history length up to (or past) threshold.
            //
            // Additionally: require at least one tournament match to
            // have been played. Without this, a session that produces
            // N hypotheses but never records a single match (e.g. the
            // ranking agent's tool call fails on every turn) has Elo
            // scores unchanged from initialization. Those unchanged
            // scores are trivially "stable" across snapshots, so the
            // termination fires before the tournament has even started.
            // Gate on match_count >= 1 to ensure the ranking has at
            // least produced one comparison before we trust stability.
            if stable
                && snap.snapshot_count + 1 >= snap.stability_threshold
                && snap.match_count >= 1
            {
                return TerminationDecision::Terminate {
                    reason: "elo_stability".to_string(),
                };
            }
        }

        TerminationDecision::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idle_policy() -> IdlePolicy {
        IdlePolicy::default()
    }

    #[test]
    fn idle_policy_no_inject_when_pending_tasks() {
        let p = idle_policy();
        let counters = RunCounters { tasks_completed: 5 };
        // Any pending task > 0 short-circuits the idle path.
        let decision = p.should_inject(counters, 1, 0, Duration::from_secs(30));
        assert_eq!(decision, InjectDecision::NoAction);
    }

    #[test]
    fn idle_policy_no_inject_when_recent_completion() {
        let p = idle_policy();
        let counters = RunCounters { tasks_completed: 5 };
        // Queue is empty but the grace window hasn't elapsed.
        let decision = p.should_inject(counters, 0, 0, Duration::from_secs(2));
        assert_eq!(decision, InjectDecision::NoAction);
    }

    #[test]
    fn idle_policy_injects_when_idle_too_long() {
        let p = idle_policy();
        let counters = RunCounters { tasks_completed: 3 };
        // Queue empty + grace elapsed + at least one task completed.
        let decision = p.should_inject(counters, 0, 0, Duration::from_secs(15));
        assert_eq!(decision, InjectDecision::SpawnReflectionAgent);
    }

    #[test]
    fn idle_policy_no_inject_when_no_tasks_completed() {
        let p = idle_policy();
        // Initial pipeline hasn't produced anything yet.
        let counters = RunCounters { tasks_completed: 0 };
        let decision = p.should_inject(counters, 0, 0, Duration::from_secs(60));
        assert_eq!(decision, InjectDecision::NoAction);
    }

    #[test]
    fn idle_policy_no_inject_when_inflight() {
        let p = idle_policy();
        let counters = RunCounters { tasks_completed: 3 };
        // A task is leased but not yet done.
        let decision = p.should_inject(counters, 0, 1, Duration::from_secs(30));
        assert_eq!(decision, InjectDecision::NoAction);
    }

    fn snapshot(
        elapsed: Duration,
        deadline: Duration,
        budget: f64,
        spent: f64,
        top: Vec<(i64, f64)>,
    ) -> RunSnapshot {
        RunSnapshot {
            elapsed,
            deadline,
            budget_usd: budget,
            budget_spent_usd: spent,
            top_hypotheses: top,
            min_hypotheses: 2,
            stability_epsilon: 25.0,
            stability_threshold: 3,
            snapshot_count: 0,
            previous_snapshot: None,
            // Default to 1 so tests that aren't exercising the
            // match_count gate don't accidentally trip it.
            match_count: 1,
        }
    }

    #[test]
    fn termination_policy_continues_under_threshold() {
        let p = TerminationPolicy::new();
        let snap = snapshot(
            Duration::from_secs(5),
            Duration::from_secs(60),
            10.0,
            1.0,
            vec![(1, 1200.0), (2, 1190.0)],
        );
        assert_eq!(p.evaluate(&snap), TerminationDecision::Continue);
    }

    #[test]
    fn termination_policy_terminates_at_budget() {
        let p = TerminationPolicy::new();
        let snap = snapshot(
            Duration::from_secs(5),
            Duration::from_secs(60),
            1.0,
            1.5,
            vec![(1, 1200.0), (2, 1190.0)],
        );
        match p.evaluate(&snap) {
            TerminationDecision::Terminate { reason } => {
                assert!(reason.starts_with("budget ("), "got {reason:?}");
            }
            other => panic!("expected Terminate, got {other:?}"),
        }
    }

    #[test]
    fn termination_policy_terminates_at_deadline() {
        let p = TerminationPolicy::new();
        let snap = snapshot(
            Duration::from_secs(120),
            Duration::from_secs(60),
            0.0,
            0.0,
            Vec::new(),
        );
        assert_eq!(
            p.evaluate(&snap),
            TerminationDecision::Terminate {
                reason: "deadline".to_string()
            }
        );
    }

    #[test]
    fn termination_policy_zero_deadline_means_no_deadline_check() {
        let p = TerminationPolicy::new();
        // deadline=Duration::ZERO is the "no deadline" sentinel.
        let snap = snapshot(
            Duration::from_secs(10_000),
            Duration::ZERO,
            0.0,
            0.0,
            Vec::new(),
        );
        assert_eq!(p.evaluate(&snap), TerminationDecision::Continue);
    }

    #[test]
    fn termination_policy_elo_stability_fires_after_threshold() {
        let p = TerminationPolicy::new();
        // Same top-2 across snapshots, stability_threshold=3 means
        // termination fires when snapshot_count + 1 >= 3.
        let make = |count: usize,
                    prev: Option<Vec<(i64, f64)>>,
                    match_count: usize| RunSnapshot {
            elapsed: Duration::from_secs(5),
            deadline: Duration::ZERO,
            budget_usd: 0.0,
            budget_spent_usd: 0.0,
            top_hypotheses: vec![(1, 1200.0), (2, 1190.0)],
            min_hypotheses: 2,
            stability_epsilon: 25.0,
            stability_threshold: 3,
            snapshot_count: count,
            previous_snapshot: prev,
            match_count,
        };

        // snapshot_count=0, no previous → not stable, no terminate.
        assert_eq!(
            p.evaluate(&make(0, None, 1)),
            TerminationDecision::Continue
        );

        // snapshot_count=1 (one prior), previous matches current, but
        // count+1 = 2 < threshold=3 → no terminate.
        assert_eq!(
            p.evaluate(&make(1, Some(vec![(1, 1200.0), (2, 1190.0)]), 1)),
            TerminationDecision::Continue
        );

        // snapshot_count=2 (two priors), previous matches current,
        // count+1 = 3 >= threshold=3 → terminate.
        assert_eq!(
            p.evaluate(&make(2, Some(vec![(1, 1200.0), (2, 1190.0)]), 1)),
            TerminationDecision::Terminate {
                reason: "elo_stability".to_string()
            }
        );

        // snapshot_count=2, previous DIFFERS from current by > epsilon → not
        // stable, no terminate.
        assert_eq!(
            p.evaluate(&make(2, Some(vec![(1, 1300.0), (2, 1190.0)]), 1)),
            TerminationDecision::Continue
        );
    }

    /// Regression: a session that produces N hypotheses but never
    /// records a single tournament match must NOT terminate on
    /// elo_stability. Without the match_count gate, the unchanged Elo
    /// scores produce trivially-stable snapshots across the idle
    /// window and the supervisor exits before the tournament has even
    /// started. This is the failure mode observed on 2026-06-30 when
    /// the ranking agent's tool call hallucinated a non-existent
    /// `memory_op` op name.
    #[test]
    fn termination_policy_elo_stability_does_not_fire_without_matches() {
        let p = TerminationPolicy::new();
        let make = |match_count: usize| RunSnapshot {
            elapsed: Duration::from_secs(60),
            deadline: Duration::ZERO,
            budget_usd: 0.0,
            budget_spent_usd: 0.0,
            top_hypotheses: vec![(1, 1200.0), (2, 1190.0)],
            min_hypotheses: 2,
            stability_epsilon: 25.0,
            stability_threshold: 3,
            snapshot_count: 2,
            previous_snapshot: Some(vec![(1, 1200.0), (2, 1190.0)]),
            match_count,
        };

        // Zero matches: even with snapshot_count=2 and a matching
        // previous snapshot, the gate refuses to terminate. Without
        // the gate this would return Terminate(elo_stability).
        assert_eq!(
            p.evaluate(&make(0)),
            TerminationDecision::Continue,
            "match_count=0 must not let elo_stability terminate",
        );

        // One match: the gate clears and the existing rule fires.
        assert_eq!(
            p.evaluate(&make(1)),
            TerminationDecision::Terminate {
                reason: "elo_stability".to_string()
            },
        );
    }
}