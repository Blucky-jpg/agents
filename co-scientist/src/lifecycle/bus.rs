//! In-process event bus. Dual-write companion to the `events` /
//! `semantic_memories` / `behavior_memories` tables: every write also
//! publishes a [`MemoryEvent`] on a `tokio::sync::broadcast` channel so a
//! External subscribers can observe live without re-querying SQLite.
//!
//! SQLite remains the source of truth for replay and post-hoc analysis.
//! The bus is a live tail; if a subscriber is slow, the older events are
//! dropped (default capacity 1024). `subscribe_replay` returns a `Receiver`
//! that the consumer can read in their own task.
//!
//! ## Failure aggregator
//!
//! [`run_failure_aggregator`] subscribes to [`MemoryEvent::MarkerFailed`]
//! events and maintains a per-(agent, op) tally. Periodically (every
//! `flush_interval`) it publishes a [`MemoryEvent::FailureStats`] on the
//! same bus with the top-N failing combinations. This closes the
//! loop that the `Runner::dispatch_marker` publish originally opened
//! (see architecture review §C4): reflection passes can now consume
//! `FailureStats` to mine which tools fail most often per agent.

use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::time::{interval, MissedTickBehavior};

/// What gets broadcast. Cheap to construct, cheap to send.
#[derive(Debug, Clone)]
pub enum MemoryEvent {
    EventLogged {
        id: i64,
        run_id: String,
        agent: String,
        type_: String,
        payload: Option<Value>,
    },
    SemanticSaved {
        id: i64,
        run_id: String,
        scope: String,
        summary: String,
    },
    BehaviorSaved {
        id: i64,
        agent: String,
        pattern: String,
    },
    TaskClaimed {
        task_id: String,
        worker_id: String,
        action: String,
    },
    TaskCompleted {
        task_id: String,
        worker_id: String,
    },
    TaskFailed {
        task_id: String,
        worker_id: String,
        error: String,
    },
    MarkerFailed {
        agent: String,
        op: String,
        error: String,
    },
    /// Aggregated failure stats for the most recent flush window.
    /// `top` is sorted descending by count, capped to `top_n` entries.
    FailureStats {
        window: Duration,
        top: Vec<FailureCount>,
        total: u64,
    },
}

/// One row in a [`MemoryEvent::FailureStats`] report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureCount {
    pub agent: String,
    pub op: String,
    pub count: u64,
}

/// A bus is a thin wrapper around a `broadcast::Sender` so the type is
/// named and swappable for tests.
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<MemoryEvent>,
}

impl std::fmt::Debug for EventBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventBus")
            .field("receiver_count", &self.tx.receiver_count())
            .finish()
    }
}

impl EventBus {
    /// Create a bus with the given channel capacity. Capacity bounds the
    /// memory footprint when no one is subscribed; older events are
    /// dropped on overflow.
    pub fn new(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity.max(1));
        Self { tx }
    }

    /// Subscribe to the live tail. The returned `Receiver` will see every
    /// event published *after* the call. Late subscribers do NOT see
    /// history — replay from SQLite for that.
    pub fn subscribe(&self) -> broadcast::Receiver<MemoryEvent> {
        self.tx.subscribe()
    }

    /// Publish an event. Returns the count of subscribers that received
    /// it (useful for tests). If no one is listening, this is a no-op
    /// and the event is dropped.
    pub fn publish(&self, ev: MemoryEvent) -> usize {
        // `send` returns Err only if there are zero receivers. Treat that
        // as a successful no-op so writers never block / fail on an empty
        // audience.
        self.tx.send(ev).unwrap_or(0)
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(1024)
    }
}

/// Configuration for [`run_failure_aggregator`].
#[derive(Debug, Clone, Copy)]
pub struct FailureAggregatorConfig {
    /// How often the aggregator flushes a `FailureStats` event onto
    /// the bus. Default: 60s.
    pub flush_interval: Duration,
    /// Cap on the number of `(agent, op)` pairs reported per flush.
    /// Default: 5.
    pub top_n: usize,
}

impl Default for FailureAggregatorConfig {
    fn default() -> Self {
        Self {
            flush_interval: Duration::from_secs(60),
            top_n: 5,
        }
    }
}

/// Run a long-lived task that aggregates [`MemoryEvent::MarkerFailed`]
/// events into a periodic [`MemoryEvent::FailureStats`] report on the
/// same bus. Closes the loop that the `Runner::dispatch_marker` publish
/// originally opened without a consumer (architecture review §C4).
///
/// Behaviour:
/// - Subscribes to `bus`; reads `MarkerFailed` events.
/// - Maintains a `HashMap<(agent, op), u64>` counter, dropping no
///   events in steady state.
/// - Every `flush_interval`, emits a `FailureStats` event with the top-N
///   `(agent, op)` pairs sorted descending by count, then resets the
///   counter for the next window.
/// - Exits cleanly when `shutdown` flips to `true`; flushes a final
///   partial window first so no failures are silently dropped.
///
/// Returns the count of flushes performed (useful for tests).
pub async fn run_failure_aggregator(
    bus: EventBus,
    config: FailureAggregatorConfig,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> u64 {
    let mut rx = bus.subscribe();
    let mut counts: HashMap<(String, String), u64> = HashMap::new();
    let mut ticker = interval(config.flush_interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut flushes: u64 = 0;

    let flush = |counts: &mut HashMap<(String, String), u64>, window: Duration, top_n: usize, bus: &EventBus| -> () {
        let total: u64 = counts.values().sum();
        let mut entries: Vec<FailureCount> = counts
            .drain()
            .map(|((agent, op), count)| FailureCount { agent, op, count })
            .collect();
        entries.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.op.cmp(&b.op)));
        entries.truncate(top_n);
        bus.publish(MemoryEvent::FailureStats {
            window,
            top: entries,
            total,
        });
    };

    loop {
        tokio::select! {
            ev = rx.recv() => {
                match ev {
                    Ok(MemoryEvent::MarkerFailed { agent, op, .. }) => {
                        *counts.entry((agent, op)).or_insert(0) += 1;
                    }
                    // Lagged: counter values for skipped events are
                    // lost. Acceptable — the per-window total is
                    // bounded, and the loss is bounded by the bus
                    // capacity (default 1024). For high-failure-rate
                    // scenarios, raise `EventBus::new` capacity.
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "failure aggregator lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    _ => {}
                }
            }
            _ = ticker.tick() => {
                flush(&mut counts, config.flush_interval, config.top_n, &bus);
                flushes += 1;
            }
            _ = shutdown.changed() => {
                // Final partial flush so the last window's data isn't
                // dropped. Window reported as the configured interval
                // because we don't track the actual elapsed time of
                // the partial window — close enough for the report.
                flush(&mut counts, config.flush_interval, config.top_n, &bus);
                flushes += 1;
                break;
            }
        }
    }
    flushes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn publish_to_no_subscribers_is_noop() {
        let bus = EventBus::new(16);
        let n = bus.publish(MemoryEvent::EventLogged {
            id: 1,
            run_id: "r".into(),
            agent: "a".into(),
            type_: "x".into(),
            payload: None,
        });
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn subscriber_receives_published_events() {
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();
        bus.publish(MemoryEvent::EventLogged {
            id: 1,
            run_id: "r".into(),
            agent: "a".into(),
            type_: "x".into(),
            payload: None,
        });
        let ev = rx.recv().await.unwrap();
        match ev {
            MemoryEvent::EventLogged { id, .. } => assert_eq!(id, 1),
            _ => panic!("wrong variant"),
        }
    }

    #[tokio::test]
    async fn multiple_subscribers_all_get_events() {
        let bus = EventBus::new(16);
        let mut a = bus.subscribe();
        let mut b = bus.subscribe();
        let n = bus.publish(MemoryEvent::SemanticSaved {
            id: 7,
            run_id: "r".into(),
            scope: "experiment".into(),
            summary: "x".to_string(),
        });
        assert_eq!(n, 2);
        let _ = a.recv().await.unwrap();
        let _ = b.recv().await.unwrap();
    }

    #[tokio::test]
    async fn aggregator_counts_marker_failed_by_agent_and_op() {
        // Drive a small number of MarkerFailed events, then trigger a
        // tick by sleeping past flush_interval. The aggregator should
        // emit a FailureStats event whose `top` matches the input.
        let bus = EventBus::new(64);
        let config = FailureAggregatorConfig {
            flush_interval: Duration::from_millis(50),
            top_n: 5,
        };
        let (sd_tx, sd_rx) = tokio::sync::watch::channel(false);

        let bus_for_sub = bus.clone();
        let h = tokio::spawn(async move {
            run_failure_aggregator(bus_for_sub, config, sd_rx).await
        });

        // Subscribe BEFORE publishing — broadcast channel does not
        // replay history. The aggregator subscribes internally so
        // we need to subscribe here too.
        let mut stats_rx = bus.subscribe();
        // Yield so the aggregator task actually attaches its
        // subscriber before we publish.
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Publish four MarkerFailed events.
        for _ in 0..3 {
            bus.publish(MemoryEvent::MarkerFailed {
                agent: "generation".into(),
                op: "save_semantic".into(),
                error: "missing field".into(),
            });
        }
        bus.publish(MemoryEvent::MarkerFailed {
            agent: "reflection".into(),
            op: "save_semantic".into(),
            error: "missing field".into(),
        });

        // Drain events until we see a NON-EMPTY FailureStats. The
        // first tick fires immediately on `interval` start (default
        // tokio behavior), so the first FailureStats we receive may
        // be the empty window — skip it.
        let mut saw_stats = false;
        let deadline = std::time::Instant::now() + Duration::from_millis(800);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(150), stats_rx.recv()).await {
                Ok(Ok(MemoryEvent::FailureStats { top, total, .. })) => {
                    if total == 0 {
                        // Empty window — the tick fired before we
                        // published. Keep draining.
                        continue;
                    }
                    assert!(!top.is_empty(), "non-empty window must have non-empty top");
                    assert_eq!(top[0].op, "save_semantic");
                    saw_stats = true;
                    break;
                }
                Ok(Ok(_)) => continue,
                _ => break,
            }
        }
        assert!(saw_stats, "expected at least one FailureStats event");

        let _ = sd_tx.send(true);
        h.await.unwrap();
    }

    #[tokio::test]
    async fn aggregator_flushes_on_shutdown() {
        // Pending counts at shutdown must be flushed, not dropped.
        let bus = EventBus::new(64);
        let config = FailureAggregatorConfig {
            flush_interval: Duration::from_secs(60), // never ticks
            top_n: 5,
        };
        let (sd_tx, sd_rx) = tokio::sync::watch::channel(false);

        let bus_for_sub = bus.clone();
        let h = tokio::spawn(async move {
            run_failure_aggregator(bus_for_sub, config, sd_rx).await
        });

        // Subscribe BEFORE publishing — broadcast channel does not
        // replay history. The aggregator subscribes internally so
        // we need to subscribe here too.
        let mut stats_rx = bus.subscribe();
        // Yield so the aggregator task actually attaches its
        // subscriber before we publish.
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        bus.publish(MemoryEvent::MarkerFailed {
            agent: "evolution".into(),
            op: "record_hypothesis".into(),
            error: "bad json".into(),
        });
        // Brief delay to let the aggregator receive and count the event.
        tokio::time::sleep(Duration::from_millis(80)).await;
        let _ = sd_tx.send(true);
        h.await.unwrap();

        // The shutdown flush must produce a FailureStats event.
        let mut saw = false;
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        while std::time::Instant::now() < deadline {
            if let Ok(ev) = tokio::time::timeout(Duration::from_millis(100), stats_rx.recv()).await {
                if let Ok(MemoryEvent::FailureStats { top, .. }) = ev {
                    if !top.is_empty() {
                        assert_eq!(top[0].agent, "evolution");
                        assert_eq!(top[0].op, "record_hypothesis");
                        assert_eq!(top[0].count, 1);
                        saw = true;
                        break;
                    }
                }
            } else {
                break;
            }
        }
        assert!(saw, "shutdown must flush pending counts");
    }

    /// Edge case: many distinct (agent, op) pairs must be reported
    /// in the top-N list, sorted descending by count.
    #[tokio::test]
    async fn aggregator_top_n_is_sorted_descending() {
        let bus = EventBus::new(64);
        let config = FailureAggregatorConfig {
            flush_interval: Duration::from_millis(40),
            top_n: 10,
        };
        let (sd_tx, sd_rx) = tokio::sync::watch::channel(false);
        let bus_for_sub = bus.clone();
        let h = tokio::spawn(async move {
            run_failure_aggregator(bus_for_sub, config, sd_rx).await
        });

        let mut stats_rx = bus.subscribe();
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Publish a varied mix.
        for _ in 0..5 {
            bus.publish(MemoryEvent::MarkerFailed {
                agent: "a".into(),
                op: "x".into(),
                error: "e".into(),
            });
        }
        for _ in 0..3 {
            bus.publish(MemoryEvent::MarkerFailed {
                agent: "b".into(),
                op: "y".into(),
                error: "e".into(),
            });
        }
        bus.publish(MemoryEvent::MarkerFailed {
            agent: "c".into(),
            op: "z".into(),
            error: "e".into(),
        });

        tokio::time::sleep(Duration::from_millis(80)).await;

        let mut saw = false;
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        while std::time::Instant::now() < deadline {
            if let Ok(Ok(MemoryEvent::FailureStats { top, .. })) =
                tokio::time::timeout(Duration::from_millis(100), stats_rx.recv()).await
            {
                if !top.is_empty() {
                    // Sorted descending by count.
                    for pair in top.windows(2) {
                        assert!(
                            pair[0].count >= pair[1].count,
                            "top list not sorted descending: {:?}",
                            top
                        );
                    }
                    saw = true;
                    break;
                }
            }
        }
        assert!(saw, "expected a FailureStats event");
        let _ = sd_tx.send(true);
        h.await.unwrap();
    }

    /// Edge case: top_n=1 caps the report to the single worst pair.
    #[tokio::test]
    async fn aggregator_top_n_caps_report_length() {
        let bus = EventBus::new(64);
        let config = FailureAggregatorConfig {
            flush_interval: Duration::from_millis(40),
            top_n: 1,
        };
        let (sd_tx, sd_rx) = tokio::sync::watch::channel(false);
        let bus_for_sub = bus.clone();
        let h = tokio::spawn(async move {
            run_failure_aggregator(bus_for_sub, config, sd_rx).await
        });

        let mut stats_rx = bus.subscribe();
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(20)).await;

        for _ in 0..3 {
            bus.publish(MemoryEvent::MarkerFailed {
                agent: "a".into(),
                op: "common".into(),
                error: "e".into(),
            });
        }
        bus.publish(MemoryEvent::MarkerFailed {
            agent: "b".into(),
            op: "rare".into(),
            error: "e".into(),
        });

        tokio::time::sleep(Duration::from_millis(80)).await;

        let mut saw = false;
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        while std::time::Instant::now() < deadline {
            if let Ok(Ok(MemoryEvent::FailureStats { top, .. })) =
                tokio::time::timeout(Duration::from_millis(100), stats_rx.recv()).await
            {
                if !top.is_empty() {
                    assert_eq!(top.len(), 1, "top_n=1 must cap report length");
                    assert_eq!(top[0].op, "common");
                    saw = true;
                    break;
                }
            }
        }
        assert!(saw);
        let _ = sd_tx.send(true);
        h.await.unwrap();
    }

    /// Edge case: empty window produces an empty `top` and zero
    /// `total`. Should not panic.
    #[tokio::test]
    async fn aggregator_empty_window_emits_zero_total() {
        let bus = EventBus::new(64);
        let config = FailureAggregatorConfig {
            flush_interval: Duration::from_millis(40),
            top_n: 5,
        };
        let (sd_tx, sd_rx) = tokio::sync::watch::channel(false);
        let bus_for_sub = bus.clone();
        let h = tokio::spawn(async move {
            run_failure_aggregator(bus_for_sub, config, sd_rx).await
        });

        let mut stats_rx = bus.subscribe();
        // Don't publish anything. Wait for the natural tick.
        tokio::time::sleep(Duration::from_millis(80)).await;

        let mut saw = false;
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        while std::time::Instant::now() < deadline {
            if let Ok(Ok(MemoryEvent::FailureStats { top, total, .. })) =
                tokio::time::timeout(Duration::from_millis(100), stats_rx.recv()).await
            {
                assert_eq!(total, 0, "no events emitted ⇒ total=0");
                assert!(top.is_empty(), "no events emitted ⇒ top empty");
                saw = true;
                break;
            }
        }
        assert!(saw, "empty window must still emit a FailureStats event");
        let _ = sd_tx.send(true);
        h.await.unwrap();
    }
}
