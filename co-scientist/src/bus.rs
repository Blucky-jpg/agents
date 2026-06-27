//! In-process event bus. Dual-write companion to the `events` /
//! `semantic_memories` / `behavior_memories` tables: every write also
//! publishes a [`MemoryEvent`] on a `tokio::sync::broadcast` channel so a
//! TUI/UI can observe live without re-querying SQLite.
//!
//! SQLite remains the source of truth for replay and post-hoc analysis.
//! The bus is a live tail; if a subscriber is slow, the older events are
//! dropped (default capacity 1024). `subscribe_replay` returns a `Receiver`
//! that the consumer can read in their own task.

use serde_json::Value;
use tokio::sync::broadcast;

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
            summary: "x".into(),
        });
        assert_eq!(n, 2);
        let _ = a.recv().await.unwrap();
        let _ = b.recv().await.unwrap();
    }
}
