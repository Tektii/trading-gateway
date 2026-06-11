//! ACK bridge for coordinating strategy acknowledgments with the Tektii Engine.
//!
//! In simulation mode, the engine waits for ACKs before advancing simulation time.
//! This bridge tracks pending `event_ids` and forwards strategy ACKs to the engine.

use std::collections::VecDeque;
use std::sync::Arc;

use async_trait::async_trait;
use tektii_gateway_core::websocket::ack::AckBridge;
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, info, trace, warn};

/// One engine event awaiting its strategy ACK, in delivery order.
#[derive(Debug)]
struct PendingEvent {
    id: String,
    /// Whether the event has actually been delivered to the strategy
    /// WebSocket (`mark_sent` flips it true).
    sent: bool,
}

/// Pending state, mutated under a single write lock so the event queue and
/// the deferred-ACK flag stay consistent across the micro-race window
/// between `connection_manager.send_to` and `mark_sent` (TEK-270).
#[derive(Debug, Default)]
struct PendingState {
    /// Pending events in registration (= engine emission = delivery) order.
    queue: VecDeque<PendingEvent>,

    /// Set when `handle_strategy_ack` finds the queue head not yet delivered.
    /// The next `mark_sent` that promotes the head consumes the flag and
    /// forwards that one event to the engine. Closes the sub-µs window where
    /// the strategy ACK arrives between `send_to.await` returning Ok and
    /// the registry's `mark_sent.await` acquiring the bridge lock.
    strategy_ack_deferred: bool,
}

impl PendingState {
    /// Pop the queue head if it has been delivered to the strategy.
    fn pop_delivered_head(&mut self) -> Option<String> {
        if self.queue.front().is_some_and(|event| event.sent) {
            self.queue.pop_front().map(|event| event.id)
        } else {
            None
        }
    }
}

/// Bridge for coordinating ACKs between strategy and engine in simulation mode.
///
/// The bridge tracks `event_ids` in two phases to avoid a race where a strategy
/// ACK arrives between WS-read and registry broadcast (TEK-270):
///
/// - `register_pending` is called when the engine emits an event (WS read).
///   The event is queued with `sent = false` — it's pending the strategy ACK
///   AND pending actual delivery to the strategy WebSocket.
/// - `mark_sent` is called by the registry's broadcast loop after
///   `connection_manager.send_to` returns Ok. Only `sent = true` events are
///   eligible to be released by a strategy ACK.
///
/// # FIFO release — one ACK frees exactly one event
///
/// Strategies ACK once per delivered event, *after* their handler for it has
/// run (SDK and canary-runner contract). Each strategy ACK therefore releases
/// exactly the **oldest delivered** event — never everything delivered. The
/// strategy still doesn't track engine `event_ids`; delivery order alone
/// correlates ACKs to events.
///
/// The previous drain-all ("auto-correlation") release was unsound: an engine
/// fill broadcasts a multi-event batch (order, trade, account, position) and
/// blocks on ALL ids, so the first strategy ACK after batch delivery — sent
/// for an event *before* the batch — released the whole batch while the
/// strategy had consumed none of it. The strategy's remaining per-event ACKs
/// then released subsequent candles it hadn't seen, letting the engine
/// advance sim time past a bar before the strategy's response orders were
/// placed. Symptom: backtest fills lagging live by a full bar and bunching
/// on one tick (TEK-1026, canary C06/C08 `fill_time` −57..−80s).
///
/// A residual sub-µs race remains between `send_to.await` returning Ok and
/// `bridge.mark_sent.await` acquiring the lock. To close it, a strategy ACK
/// that finds the queue head still `sent = false` sets
/// `strategy_ack_deferred = true`. The next `mark_sent` that promotes the
/// head consumes the flag and forwards that event synchronously inside the
/// same critical section.
///
/// # Flow
///
/// 1. Engine sends event with `event_id` to gateway WS read.
/// 2. Gateway calls `register_pending(id)` → queued, `sent = false`.
/// 3. Registry forwards event through pipeline.
/// 4. Registry calls `connection_manager.send_to(conn_id, ws_msg)` for each
///    strategy. On Ok, registry calls `mark_sent(id)` → `sent = true`.
/// 5. Strategy processes event and sends `EventAck`.
/// 6. Gateway calls `handle_strategy_ack()` → releases the oldest
///    `sent = true` event.
/// 7. Engine receives ACK and advances simulation time once every id it
///    waits on has been released.
#[derive(Debug)]
pub struct TektiiAckBridge {
    /// Pending event queue + deferred-ACK flag, both mutated atomically.
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
    /// The event is queued with `sent = false` — eligible for release only
    /// after `mark_sent` is called once the registry has actually delivered it
    /// to the strategy WebSocket.
    pub async fn register_pending(&self, event_id: String) {
        let mut state = self.state.write().await;
        state.queue.push_back(PendingEvent {
            id: event_id.clone(),
            sent: false,
        });
        debug!(event_id = %event_id, pending_count = state.queue.len(), "Registered pending event");
    }

    /// Mark events as actually delivered to the strategy WebSocket
    /// (phase 2: post `connection_manager.send_to`).
    ///
    /// If a strategy ACK was previously deferred (it arrived before the queue
    /// head completed broadcast), a promotion of the head consumes the flag
    /// and forwards that one event to the engine — closing the micro-race
    /// between `send_to.await` and `mark_sent.await`.
    ///
    /// Unknown `event_ids` are silently ignored — defensive against ordering
    /// edge cases (late `mark_sent` after an early release, duplicate calls).
    pub async fn mark_sent(&self, event_ids: Vec<String>) {
        if event_ids.is_empty() {
            return;
        }

        let mut state = self.state.write().await;
        let mut newly_sent = 0_usize;

        for id in &event_ids {
            if let Some(event) = state.queue.iter_mut().find(|event| event.id == *id) {
                if !event.sent {
                    event.sent = true;
                    newly_sent += 1;
                }
            } else {
                trace!(event_id = %id, "mark_sent for unknown event_id (already released?)");
            }
        }

        if newly_sent > 0 {
            debug!(count = newly_sent, "Marked events as sent to strategy");
        }

        // If a strategy ACK was deferred and this call promoted the queue
        // head, consume the deferred ACK now: release exactly that one event.
        // Same critical section, no yield.
        if state.strategy_ack_deferred
            && let Some(id) = state.pop_delivered_head()
        {
            state.strategy_ack_deferred = false;
            let in_flight = state.queue.len();
            drop(state);
            if self.engine_ack_tx.send(vec![id]).is_err() {
                warn!("Engine ACK channel closed, dropping deferred ACK");
            } else {
                info!(in_flight, "Forwarded deferred strategy ACK to engine");
            }
        }
    }

    /// Handle strategy ACK by releasing the oldest delivered event and
    /// forwarding it to the engine.
    ///
    /// Strategies ACK once per delivered event, after handling it, so each
    /// ACK releases exactly one event — the queue head — never everything
    /// delivered. Releasing more would tell the engine the strategy has
    /// processed events still sitting unconsumed in its buffer, letting sim
    /// time advance before the strategy's response orders are placed
    /// (TEK-1026). If the head is still in flight (the micro-race between
    /// `send_to` and `mark_sent`), the ACK is deferred and consumed by the
    /// `mark_sent` that promotes the head.
    pub async fn handle_strategy_ack(&self) {
        let mut state = self.state.write().await;

        if state.queue.is_empty() {
            // No events tracked at all — strategy ACK'd something the bridge
            // doesn't know about. Don't defer (no future mark_sent is owed
            // for an event we never registered).
            warn!("Strategy ACK received but no pending events to forward");
            return;
        }

        if let Some(id) = state.pop_delivered_head() {
            // Released the head — clear any deferred flag (this ACK
            // satisfies it) and forward.
            state.strategy_ack_deferred = false;
            let in_flight = state.queue.len();
            drop(state);

            if self.engine_ack_tx.send(vec![id]).is_err() {
                warn!("Engine ACK channel closed, dropping ACK");
            } else {
                info!(in_flight, "Forwarded strategy ACK to engine");
            }
        } else {
            // The oldest pending event is still in flight (`sent = false`).
            // Defer the ACK — the `mark_sent` that promotes the head will
            // consume the flag and forward to the engine.
            state.strategy_ack_deferred = true;
            debug!(
                in_flight = state.queue.len(),
                "Strategy ACK deferred — oldest pending event not yet delivered"
            );
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
        self.state.read().await.queue.len()
    }
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
    async fn test_handle_strategy_ack_forwards_only_oldest_delivered_event() {
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

        // One strategy ACK = one consumed event. Only the oldest delivered
        // event may be released — evt-1/evt-2 are still unconsumed in the
        // strategy's buffer.
        bridge.handle_strategy_ack().await;

        let acked = rx.recv().await.expect("Should receive ACK");
        assert_eq!(acked, vec!["evt-0".to_string()]);
        assert!(
            rx.try_recv().is_err(),
            "Only the oldest delivered event may be released per ACK"
        );
        assert_eq!(bridge.pending_count().await, 2);

        // Subsequent ACKs drain the rest, one per ACK, in delivery order.
        bridge.handle_strategy_ack().await;
        assert_eq!(
            rx.recv().await.expect("second ACK"),
            vec!["evt-1".to_string()]
        );
        bridge.handle_strategy_ack().await;
        assert_eq!(
            rx.recv().await.expect("third ACK"),
            vec!["evt-2".to_string()]
        );
        assert_eq!(bridge.pending_count().await, 0);
    }

    /// TEK-1026 regression: an engine fill broadcasts a multi-event batch
    /// (order, trade, account, position) and waits for ALL ids before
    /// advancing sim time. Pre-FIFO, the first strategy ACK after batch
    /// delivery drained the whole batch — the engine resumed while the
    /// strategy had consumed none of it, and the strategy's remaining
    /// per-event ACKs then released subsequent candles the strategy hadn't
    /// seen. Net effect: candle-gated orders reached the engine one bar
    /// late (C06/C08 fill_time −57..−80s, bunched same-tick fills).
    ///
    /// FIFO contract: the engine's batch wait completes only after the
    /// strategy has acked every event in the batch — i.e. after it has
    /// actually processed all of them.
    #[tokio::test]
    async fn test_fill_batch_releases_one_event_per_ack_in_delivery_order() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let bridge = TektiiAckBridge::new(tx);

        // Fill batch (4 events) followed by the next candle, all delivered
        // to the strategy WebSocket back-to-back.
        let ids = ["order", "trade", "account", "position", "candle"];
        for id in ids {
            bridge.register_pending(id.to_string()).await;
        }
        bridge
            .mark_sent(ids.iter().map(ToString::to_string).collect())
            .await;

        // The strategy consumes + acks one event at a time. Each ACK must
        // release exactly that event — the candle (last) is only released
        // by the fifth ACK, never by a stale batch ACK.
        for expected in ids {
            bridge.handle_strategy_ack().await;
            assert_eq!(
                rx.recv().await.expect("ACK forwarded"),
                vec![expected.to_string()],
                "each strategy ACK must release exactly the oldest delivered event"
            );
        }
        assert_eq!(bridge.pending_count().await, 0);
    }

    /// Strict head-of-line: an ACK must never release a later event past an
    /// undelivered head, even if that later event is already marked sent.
    /// Delivery order defines consumption order — releasing out of order
    /// would ack an event the strategy cannot have processed yet.
    #[tokio::test]
    async fn test_ack_never_releases_past_undelivered_head() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let bridge = TektiiAckBridge::new(tx);

        bridge.register_pending("evt-a".to_string()).await;
        bridge.register_pending("evt-b".to_string()).await;
        // Out-of-order bookkeeping: only the later event is marked sent.
        bridge.mark_sent(vec!["evt-b".to_string()]).await;

        // Head (evt-a) is undelivered — the ACK defers; evt-b must NOT leak.
        bridge.handle_strategy_ack().await;
        assert!(
            rx.try_recv().is_err(),
            "ACK must not release evt-b past the undelivered head evt-a"
        );
        assert_eq!(bridge.pending_count().await, 2);

        // Head delivery consumes the deferred ACK and releases evt-a only.
        bridge.mark_sent(vec!["evt-a".to_string()]).await;
        assert_eq!(
            rx.recv().await.expect("deferred ACK"),
            vec!["evt-a".to_string()]
        );
        assert_eq!(bridge.pending_count().await, 1);

        // evt-b still needs its own ACK.
        bridge.handle_strategy_ack().await;
        assert_eq!(
            rx.recv().await.expect("evt-b ACK"),
            vec!["evt-b".to_string()]
        );
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

        // Second batch — two delivered events take two ACKs, one each.
        bridge.register_pending("evt-1".to_string()).await;
        bridge.register_pending("evt-2".to_string()).await;
        bridge
            .mark_sent(vec!["evt-1".to_string(), "evt-2".to_string()])
            .await;
        bridge.handle_strategy_ack().await;
        assert_eq!(
            rx.recv().await.expect("Should receive evt-1"),
            vec!["evt-1".to_string()]
        );
        bridge.handle_strategy_ack().await;
        assert_eq!(
            rx.recv().await.expect("Should receive evt-2"),
            vec!["evt-2".to_string()]
        );

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
    /// to one — at most one legitimate ACK can sit in the send_to/mark_sent
    /// micro-race window at a time, so repeated ACKs in the deferred state
    /// are idempotent.
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
