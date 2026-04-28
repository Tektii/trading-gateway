//! ACK bridge for coordinating strategy acknowledgments with the Tektii Engine.
//!
//! In simulation mode, the engine waits for ACKs before advancing simulation time.
//! This bridge tracks pending `event_ids` and forwards strategy ACKs to the engine.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tektii_gateway_core::websocket::ack::AckBridge;
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, info, trace, warn};

/// Pending state, mutated under a single write lock so the events map and
/// the deferred-ACK flag stay consistent across the micro-race window
/// between `connection_manager.send_to` and `mark_sent` (TEK-270).
#[derive(Debug, Default)]
struct PendingState {
    /// Pending `event_ids` keyed by id; the bool tracks whether the event
    /// has actually been delivered to the strategy WebSocket
    /// (`mark_sent` flips it true).
    events: HashMap<String, bool>,

    /// Set when `handle_strategy_ack` finds nothing to drain but there are
    /// `sent = false` events still in flight. The next `mark_sent` that
    /// promotes an entry to `sent = true` consumes the flag and forwards
    /// the drained events to the engine. Closes the sub-µs window where
    /// the strategy ACK arrives between `send_to.await` returning Ok and
    /// the registry's `mark_sent.await` acquiring the bridge lock.
    strategy_ack_deferred: bool,
}

/// Bridge for coordinating ACKs between strategy and engine in simulation mode.
///
/// The bridge tracks `event_ids` in two phases to avoid a race where a strategy
/// ACK arrives between WS-read and registry broadcast (TEK-270):
///
/// - `register_pending` is called when the engine emits an event (WS read).
///   The event is tracked with `sent = false` — it's pending the strategy ACK
///   AND pending actual delivery to the strategy WebSocket.
/// - `mark_sent` is called by the registry's broadcast loop after
///   `connection_manager.send_to` returns Ok. Only `sent = true` events are
///   eligible to be drained on the next strategy ACK.
///
/// When a strategy sends an ACK, all `sent = true` events are drained and
/// ACK'd to the engine (auto-correlation approach). `sent = false` events
/// remain pending and will be drained by a later strategy ACK once they are
/// actually delivered.
///
/// A residual sub-µs race remains between `send_to.await` returning Ok and
/// `bridge.mark_sent.await` acquiring the lock. To close it, an out-of-order
/// strategy ACK that finds no `sent = true` entries (but DOES find
/// `sent = false` entries) sets `strategy_ack_deferred = true`. The next
/// `mark_sent` that promotes an entry consumes the flag and drains
/// synchronously inside the same critical section.
///
/// # Flow
///
/// 1. Engine sends event with `event_id` to gateway WS read.
/// 2. Gateway calls `register_pending(id)` → tracked, `sent = false`.
/// 3. Registry forwards event through pipeline.
/// 4. Registry calls `connection_manager.send_to(conn_id, ws_msg)` for each
///    strategy. On Ok, registry calls `mark_sent(id)` → `sent = true`.
/// 5. Strategy processes event and sends `EventAck`.
/// 6. Gateway calls `handle_strategy_ack()` → drains only `sent = true` events.
/// 7. Engine receives ACK and advances simulation time.
#[derive(Debug)]
pub struct TektiiAckBridge {
    /// Pending events + deferred-ACK flag, both mutated atomically.
    state: Arc<RwLock<PendingState>>,

    /// Channel to send ACKs to engine writer task.
    engine_ack_tx: mpsc::UnboundedSender<Vec<String>>,
}

impl TektiiAckBridge {
    /// Create a new `TektiiAckBridge` along with the channel receiver.
    ///
    /// This is the preferred way to create a bridge. The returned receiver should
    /// be passed to the WebSocket provider's connect method, while the bridge
    /// itself should be registered for strategy ACK routing.
    ///
    /// # Returns
    ///
    /// A tuple of `(Arc<TektiiAckBridge>, UnboundedReceiver<Vec<String>>)`:
    /// - The bridge for tracking pending events and handling strategy ACKs
    /// - The receiver for forwarding ACKs to the engine WebSocket writer task
    #[must_use]
    pub fn create() -> (Arc<Self>, mpsc::UnboundedReceiver<Vec<String>>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let bridge = Arc::new(Self {
            state: Arc::new(RwLock::new(PendingState::default())),
            engine_ack_tx: tx,
        });
        (bridge, rx)
    }

    /// Create a new `TektiiAckBridge` from an existing sender.
    ///
    /// Use `create()` instead unless you have a specific reason to provide your own channel.
    #[must_use]
    pub fn new(engine_ack_tx: mpsc::UnboundedSender<Vec<String>>) -> Self {
        Self {
            state: Arc::new(RwLock::new(PendingState::default())),
            engine_ack_tx,
        }
    }

    /// Register an event as pending strategy ACK (phase 1: WS read time).
    ///
    /// The event is recorded with `sent = false` — eligible to be drained only
    /// after `mark_sent` is called once the registry has actually delivered it
    /// to the strategy WebSocket.
    pub async fn register_pending(&self, event_id: String) {
        let mut state = self.state.write().await;
        state.events.insert(event_id.clone(), false);
        debug!(event_id = %event_id, pending_count = state.events.len(), "Registered pending event");
    }

    /// Mark events as actually delivered to the strategy WebSocket
    /// (phase 2: post `connection_manager.send_to`).
    ///
    /// If a strategy ACK was previously deferred (it arrived before any event
    /// completed broadcast), the events promoted by this call are drained
    /// synchronously and forwarded to the engine — closing the micro-race
    /// between `send_to.await` and `mark_sent.await`.
    ///
    /// Unknown `event_ids` are silently ignored — defensive against ordering
    /// edge cases (late `mark_sent` after an early drain, duplicate calls).
    pub async fn mark_sent(&self, event_ids: Vec<String>) {
        if event_ids.is_empty() {
            return;
        }

        let mut state = self.state.write().await;
        let mut newly_sent = 0_usize;

        for id in &event_ids {
            if let Some(sent) = state.events.get_mut(id) {
                if !*sent {
                    *sent = true;
                    newly_sent += 1;
                }
            } else {
                trace!(event_id = %id, "mark_sent for unknown event_id (already drained?)");
            }
        }

        if newly_sent > 0 {
            debug!(count = newly_sent, "Marked events as sent to strategy");
        }

        // If a strategy ACK was deferred, consume it now: drain any
        // `sent = true` entries (which now includes the events we just
        // promoted) and forward to engine. Same critical section, no yield.
        if state.strategy_ack_deferred && newly_sent > 0 {
            let to_ack = drain_sent(&mut state.events);
            if !to_ack.is_empty() {
                state.strategy_ack_deferred = false;
                let count = to_ack.len();
                let in_flight = state.events.len();
                drop(state);
                if self.engine_ack_tx.send(to_ack).is_err() {
                    warn!("Engine ACK channel closed, dropping deferred ACK");
                } else {
                    info!(
                        count,
                        in_flight, "Forwarded deferred strategy ACK to engine"
                    );
                }
            }
        }
    }

    /// Handle strategy ACK by draining `sent = true` events and forwarding to
    /// engine.
    ///
    /// Auto-correlation: ANY strategy ACK drains every event already delivered
    /// to the strategy (no need for the strategy to track engine `event_ids`).
    /// Events that have been registered but NOT yet delivered (`sent = false`)
    /// remain pending. If the ACK arrives while every pending event is still
    /// in flight (the micro-race between `send_to` and `mark_sent`), the ACK
    /// is deferred and consumed by the next `mark_sent` that promotes an
    /// entry.
    pub async fn handle_strategy_ack(&self) {
        let mut state = self.state.write().await;

        if state.events.is_empty() {
            // No events tracked at all — strategy ACK'd something the bridge
            // doesn't know about. Don't defer (no future mark_sent is owed
            // for an event we never registered).
            warn!("Strategy ACK received but no pending events to forward");
            return;
        }

        let to_ack = drain_sent(&mut state.events);

        if to_ack.is_empty() {
            // Every pending event is still in flight (`sent = false`).
            // Defer the ACK — the next `mark_sent` that promotes an entry
            // will consume the flag and forward to the engine.
            state.strategy_ack_deferred = true;
            debug!(
                in_flight = state.events.len(),
                "Strategy ACK deferred — no events delivered yet"
            );
            return;
        }

        // Got some sent entries to drain — clear any deferred flag (this ACK
        // satisfies it) and forward.
        state.strategy_ack_deferred = false;
        let count = to_ack.len();
        let in_flight = state.events.len();
        drop(state);

        if self.engine_ack_tx.send(to_ack).is_err() {
            warn!("Engine ACK channel closed, dropping ACKs");
        } else {
            info!(count, in_flight, "Forwarded strategy ACK to engine");
        }
    }

    /// Immediately ACK an event to the engine (bypasses pending tracking).
    ///
    /// Called when an event does NOT match the subscription filter. The event
    /// is ACK'd immediately without waiting for strategy acknowledgment.
    pub fn immediate_ack(&self, event_id: String) {
        debug!(event_id = %event_id, "Immediate ACK (not subscribed)");

        if self.engine_ack_tx.send(vec![event_id]).is_err() {
            // Channel closed - engine writer task has stopped
            trace!("Engine ACK channel closed, dropping immediate ACK");
        }
    }

    /// Get the number of pending events (for testing/debugging).
    #[cfg(test)]
    pub async fn pending_count(&self) -> usize {
        self.state.read().await.events.len()
    }
}

/// Drain `sent = true` entries from the events map, returning their ids.
fn drain_sent(events: &mut HashMap<String, bool>) -> Vec<String> {
    let mut to_ack: Vec<String> = Vec::new();
    events.retain(|id, sent| {
        if *sent {
            to_ack.push(id.clone());
            false
        } else {
            true
        }
    });
    to_ack
}

#[async_trait]
impl AckBridge for TektiiAckBridge {
    async fn handle_strategy_ack(&self) {
        self.handle_strategy_ack().await;
    }

    async fn register_pending(&self, event_ids: Vec<String>) {
        for id in event_ids {
            self.register_pending(id).await;
        }
    }

    async fn mark_sent(&self, event_ids: Vec<String>) {
        self.mark_sent(event_ids).await;
    }

    async fn immediate_ack(&self, event_ids: Vec<String>) {
        for id in event_ids {
            self.immediate_ack(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_register_pending_tracks_event() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let bridge = TektiiAckBridge::new(tx);

        bridge.register_pending("evt-0".to_string()).await;
        bridge.register_pending("evt-1".to_string()).await;

        assert_eq!(bridge.pending_count().await, 2);
    }

    #[tokio::test]
    async fn test_handle_strategy_ack_forwards_to_engine() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let bridge = TektiiAckBridge::new(tx);

        // Register some pending events and mark them all as actually sent
        // to the strategy.
        bridge.register_pending("evt-0".to_string()).await;
        bridge.register_pending("evt-1".to_string()).await;
        bridge.register_pending("evt-2".to_string()).await;
        bridge
            .mark_sent(vec![
                "evt-0".to_string(),
                "evt-1".to_string(),
                "evt-2".to_string(),
            ])
            .await;

        // Handle strategy ACK
        bridge.handle_strategy_ack().await;

        // Verify all events were forwarded
        let acked = rx.recv().await.expect("Should receive ACKs");
        assert_eq!(acked.len(), 3);
        assert!(acked.contains(&"evt-0".to_string()));
        assert!(acked.contains(&"evt-1".to_string()));
        assert!(acked.contains(&"evt-2".to_string()));

        // Pending should be empty now
        assert_eq!(bridge.pending_count().await, 0);
    }

    #[tokio::test]
    async fn test_handle_strategy_ack_does_nothing_when_empty() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let bridge = TektiiAckBridge::new(tx);

        // Handle ACK with no pending events
        bridge.handle_strategy_ack().await;

        // Should not send anything
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_immediate_ack_bypasses_pending() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let bridge = TektiiAckBridge::new(tx);

        // Register a pending event
        bridge.register_pending("evt-0".to_string()).await;

        // Immediate ACK for different event
        bridge.immediate_ack("evt-1".to_string());

        // Should receive the immediate ACK
        let acked = rx.recv().await.expect("Should receive immediate ACK");
        assert_eq!(acked, vec!["evt-1".to_string()]);

        // Pending event should still be there
        assert_eq!(bridge.pending_count().await, 1);
    }

    #[tokio::test]
    async fn test_multiple_acks_drain_correctly() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let bridge = TektiiAckBridge::new(tx);

        // First batch — needs both register_pending + mark_sent before drain.
        bridge.register_pending("evt-0".to_string()).await;
        bridge.mark_sent(vec!["evt-0".to_string()]).await;
        bridge.handle_strategy_ack().await;

        let batch1 = rx.recv().await.expect("Should receive first batch");
        assert_eq!(batch1.len(), 1);

        // Second batch
        bridge.register_pending("evt-1".to_string()).await;
        bridge.register_pending("evt-2".to_string()).await;
        bridge
            .mark_sent(vec!["evt-1".to_string(), "evt-2".to_string()])
            .await;
        bridge.handle_strategy_ack().await;

        let batch2 = rx.recv().await.expect("Should receive second batch");
        assert_eq!(batch2.len(), 2);

        // Pending should be empty
        assert_eq!(bridge.pending_count().await, 0);
    }

    /// Race regression (TEK-270): if a strategy ACK arrives between
    /// `register_pending` (engine WS read) and `mark_sent` (registry post
    /// `send_to`), the engine must NOT receive ACK for events still in flight
    /// to the strategy WebSocket.
    #[tokio::test]
    async fn test_strategy_ack_skips_pending_events_not_yet_sent() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let bridge = TektiiAckBridge::new(tx);

        // Engine emits two events back-to-back. Both are registered pending
        // immediately at WS-read time.
        bridge.register_pending("evt-1".to_string()).await;
        bridge.register_pending("evt-2".to_string()).await;

        // Only evt-1 has actually completed `connection_manager.send_to`.
        // evt-2 is still queued in the registry pipeline.
        bridge.mark_sent(vec!["evt-1".to_string()]).await;

        // Strategy ACKs (it has only seen evt-1).
        bridge.handle_strategy_ack().await;

        // Engine ACK must NOT include evt-2.
        let acked = rx.recv().await.expect("Should receive engine ACK");
        assert_eq!(acked, vec!["evt-1".to_string()]);
        assert!(rx.try_recv().is_err(), "Only one batch should be sent");

        // evt-2 stays pending until it's actually sent.
        assert_eq!(bridge.pending_count().await, 1);

        // Once registry finishes broadcasting evt-2, mark_sent fires.
        bridge.mark_sent(vec!["evt-2".to_string()]).await;

        // Next strategy ACK drains evt-2.
        bridge.handle_strategy_ack().await;
        let acked = rx.recv().await.expect("Should receive engine ACK");
        assert_eq!(acked, vec!["evt-2".to_string()]);
        assert_eq!(bridge.pending_count().await, 0);
    }

    /// Strategy ACK arriving before any event has been delivered (`mark_sent`
    /// never called) must be a no-op for the engine — pending events remain
    /// in flight.
    #[tokio::test]
    async fn test_strategy_ack_with_no_sent_events_is_noop() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let bridge = TektiiAckBridge::new(tx);

        bridge.register_pending("evt-1".to_string()).await;
        bridge.register_pending("evt-2".to_string()).await;

        // Strategy ACK arrives but nothing has been marked sent yet.
        bridge.handle_strategy_ack().await;

        // No engine ACK should fire — events still in flight.
        assert!(
            rx.try_recv().is_err(),
            "Engine must not receive ACK for events still in flight"
        );

        // Both events remain pending.
        assert_eq!(bridge.pending_count().await, 2);
    }

    /// `mark_sent` for an unknown event_id is silently ignored (no panic).
    /// Defensive against ordering edge cases (e.g., late mark_sent after a
    /// drain, or duplicate mark_sent).
    #[tokio::test]
    async fn test_mark_sent_unknown_event_id_is_ignored() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let bridge = TektiiAckBridge::new(tx);

        // Should not panic
        bridge.mark_sent(vec!["unknown".to_string()]).await;

        assert_eq!(bridge.pending_count().await, 0);
    }

    /// TEK-270 micro-race: a strategy ACK can arrive between the registry's
    /// `connection_manager.send_to(...).await` returning Ok and the
    /// subsequent `bridge.mark_sent(...).await`. With one-event-per-tick
    /// engine pacing this would deadlock — strategy already ACK'd the event
    /// it just received, but the bridge had nothing marked sent at the
    /// moment, so no engine ACK fires; engine waits forever for an ACK that
    /// already happened.
    ///
    /// Fix: `handle_strategy_ack` records that an ACK is owed when it finds
    /// nothing to drain. The next `mark_sent` that promotes an entry to
    /// `sent = true` consumes the deferred ACK and forwards to the engine.
    #[tokio::test]
    async fn test_deferred_strategy_ack_is_drained_by_mark_sent() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let bridge = TektiiAckBridge::new(tx);

        // Engine emits an event, registered pending at WS-read time.
        bridge.register_pending("evt-1".to_string()).await;

        // Race: strategy ACK arrives before registry's `mark_sent` fires.
        // (In production this is the sub-µs window between send_to and
        // mark_sent.) Bridge sees nothing sent yet — defer the ACK.
        bridge.handle_strategy_ack().await;

        // No engine ACK yet — there was nothing to drain.
        assert!(
            rx.try_recv().is_err(),
            "No engine ACK should fire while event is in flight"
        );

        // Registry now calls mark_sent. The deferred ACK fires immediately,
        // forwarding to engine.
        bridge.mark_sent(vec!["evt-1".to_string()]).await;

        let acked = rx
            .recv()
            .await
            .expect("Deferred strategy ACK should fire on mark_sent");
        assert_eq!(acked, vec!["evt-1".to_string()]);
        assert_eq!(
            bridge.pending_count().await,
            0,
            "evt-1 should be drained from pending"
        );
    }

    /// Deferred ACK is consumed once. Subsequent `mark_sent` calls that
    /// promote different events should NOT auto-fire an engine ACK — the
    /// engine paces on a per-strategy-ACK basis, not per `mark_sent`.
    #[tokio::test]
    async fn test_deferred_ack_is_consumed_only_once() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let bridge = TektiiAckBridge::new(tx);

        bridge.register_pending("evt-1".to_string()).await;
        bridge.register_pending("evt-2".to_string()).await;

        // Strategy ACKs while both are in flight — defer.
        bridge.handle_strategy_ack().await;
        assert!(rx.try_recv().is_err());

        // First mark_sent consumes the deferred ACK.
        bridge.mark_sent(vec!["evt-1".to_string()]).await;
        let acked = rx.recv().await.expect("First drain");
        assert_eq!(acked, vec!["evt-1".to_string()]);

        // Second mark_sent must NOT auto-fire — no strategy ACK is owed.
        bridge.mark_sent(vec!["evt-2".to_string()]).await;
        assert!(
            rx.try_recv().is_err(),
            "Second mark_sent must not drain without a fresh strategy ACK"
        );

        // Pending state: evt-2 still waiting for the next strategy ACK.
        assert_eq!(bridge.pending_count().await, 1);

        // Next strategy ACK drains evt-2.
        bridge.handle_strategy_ack().await;
        let acked = rx.recv().await.expect("Second strategy ACK drains evt-2");
        assert_eq!(acked, vec!["evt-2".to_string()]);
    }

    /// Multiple strategy ACKs deferred while events are in flight collapse
    /// to one — auto-correlation already drains everything available, so
    /// repeated ACKs in the deferred state are idempotent.
    #[tokio::test]
    async fn test_multiple_deferred_acks_are_idempotent() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let bridge = TektiiAckBridge::new(tx);

        bridge.register_pending("evt-1".to_string()).await;

        // Three strategy ACKs while evt-1 is in flight.
        bridge.handle_strategy_ack().await;
        bridge.handle_strategy_ack().await;
        bridge.handle_strategy_ack().await;
        assert!(rx.try_recv().is_err());

        // mark_sent consumes the deferred state once.
        bridge.mark_sent(vec!["evt-1".to_string()]).await;
        let acked = rx.recv().await.expect("Should fire once");
        assert_eq!(acked, vec!["evt-1".to_string()]);

        // No further ACKs — repeated handle_strategy_ack collapsed to one.
        assert!(rx.try_recv().is_err());
    }
}
