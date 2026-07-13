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
    /// Whether the strategy has already ACKed this event. Only meaningful
    /// while `sent` is false: the ACK arrived inside the sub-µs window
    /// between `send_to.await` returning Ok and the registry's
    /// `mark_sent.await` acquiring the bridge lock. The `mark_sent` that
    /// confirms delivery releases the event immediately.
    acked: bool,
}

/// Pending state, mutated under a single write lock so ACK arrival and
/// delivery confirmation stay consistent across the micro-race window
/// between `connection_manager.send_to` and `mark_sent`.
#[derive(Debug, Default)]
struct PendingState {
    /// Pending events in registration (= engine emission) order. Order is
    /// diagnostic only — release is by `event_id`, never by position.
    queue: VecDeque<PendingEvent>,
}

/// Bridge for coordinating ACKs between strategy and engine in simulation mode.
///
/// The bridge tracks `event_ids` in two phases to avoid a race where a strategy
/// ACK arrives between WS-read and registry broadcast:
///
/// - `register_pending` is called when the engine emits an event (WS read).
///   The event is queued with `sent = false` — it's pending the strategy ACK
///   AND pending actual delivery to the strategy WebSocket.
/// - `mark_sent` is called by the registry's broadcast loop after
///   `connection_manager.send_to` returns Ok. Only `sent = true` events are
///   released to the engine on ACK.
///
/// # Strict-ID release — the ACK names what it acknowledges
///
/// The gateway echoes the engine `event_id` on every outbound engine frame;
/// the SDK returns it in `events_processed`. `handle_strategy_ack` releases
/// exactly the ids the ACK names — never by queue position. Consequences:
///
/// - An empty ACK (gateway-local events — connection / data-freshness /
///   error / rate-limit — carry no engine id) releases nothing, so it can
///   never over-pop the engine ahead of the strategy.
/// - An ACK naming an unknown id releases nothing — a confused client
///   cannot advance the sim.
/// - An ACK for a delivered event releases it even when an older event was
///   never delivered: a delivery loss costs the engine one ACK-window
///   timeout for the lost event instead of permanently wedging every later
///   ACK behind it. The engine matches ACKs by exact id (accumulating
///   across frames), so out-of-order release is safe.
/// - Duplicate ACKs are idempotent: the id is gone after the first release.
///
/// The previous positional release ("auto-correlation") required this queue
/// to mirror the engine's send order perfectly across every delivery
/// failure; each drift (over-pop, offset, wedge) surfaced as a distant,
/// hard-to-diagnose halt or 0-trade run.
///
/// A residual sub-µs race remains between `send_to.await` returning Ok and
/// `bridge.mark_sent.await` acquiring the lock: the strategy's ACK can name
/// an event not yet marked sent. The ACK is recorded on the entry
/// (`acked = true`) and the `mark_sent` that confirms delivery releases it
/// inside the same critical section.
///
/// # Flow
///
/// 1. Engine sends event with `event_id` to gateway WS read.
/// 2. Gateway calls `register_pending(id)` → queued, `sent = false`.
/// 3. Registry forwards event through pipeline; the outbound frame carries
///    the echoed `event_id`.
/// 4. Registry calls `connection_manager.send_to(conn_id, ws_msg)` for each
///    strategy. On Ok, registry calls `mark_sent(id)` → `sent = true`.
/// 5. Strategy processes the event and sends `EventAck` echoing the id.
/// 6. Gateway calls `handle_strategy_ack(ids)` → releases exactly those
///    events.
/// 7. Engine receives ACK and advances simulation time once every id it
///    waits on has been released.
#[derive(Debug)]
pub struct TektiiAckBridge {
    /// Pending event queue, ACK state and delivery state mutated atomically.
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
            acked: false,
        });
        debug!(event_id = %event_id, pending_count = state.queue.len(), "Registered pending event");
    }

    /// Mark events as actually delivered to the strategy WebSocket
    /// (phase 2: post `connection_manager.send_to`).
    ///
    /// An event whose strategy ACK already arrived (the micro-race between
    /// `send_to.await` and `mark_sent.await`) is released to the engine
    /// immediately, inside the same critical section.
    ///
    /// Unknown `event_ids` are silently ignored — defensive against ordering
    /// edge cases (late `mark_sent` after an early release, duplicate calls).
    pub async fn mark_sent(&self, event_ids: Vec<String>) {
        if event_ids.is_empty() {
            return;
        }

        let mut state = self.state.write().await;
        let mut newly_sent = 0_usize;
        let mut released: Vec<String> = Vec::new();

        for id in &event_ids {
            let Some(idx) = state.queue.iter().position(|event| event.id == *id) else {
                trace!(event_id = %id, "mark_sent for unknown event_id (already released?)");
                continue;
            };
            let event = &mut state.queue[idx];
            if !event.sent {
                event.sent = true;
                newly_sent += 1;
            }
            // The strategy's ACK beat this delivery confirmation — release
            // now that delivery is confirmed.
            if event.acked
                && let Some(event) = state.queue.remove(idx)
            {
                released.push(event.id);
            }
        }

        if newly_sent > 0 {
            debug!(count = newly_sent, "Marked events as sent to strategy");
        }

        if !released.is_empty() {
            let in_flight = state.queue.len();
            drop(state);
            if self.engine_ack_tx.send(released).is_err() {
                warn!("Engine ACK channel closed, dropping deferred ACK");
            } else {
                info!(in_flight, "Forwarded deferred strategy ACK to engine");
            }
        }
    }

    /// Handle a strategy ACK by releasing exactly the events it names and
    /// forwarding them to the engine (strict-ID release).
    ///
    /// For each acked id: a delivered (`sent = true`) event is released and
    /// forwarded; an event still in flight (the micro-race between `send_to`
    /// and `mark_sent`) is marked `acked` and released by the `mark_sent`
    /// that confirms its delivery; an unknown id — gateway-local event,
    /// duplicate ACK, or a client echoing something the bridge never
    /// registered — releases nothing. Empty ACKs are a no-op by
    /// construction.
    pub async fn handle_strategy_ack(&self, event_ids: &[String]) {
        if event_ids.is_empty() {
            trace!("Empty strategy ACK (gateway-local event, no engine id) — nothing to release");
            return;
        }

        let mut state = self.state.write().await;
        let mut released: Vec<String> = Vec::new();

        for id in event_ids {
            let Some(idx) = state.queue.iter().position(|event| event.id == *id) else {
                warn!(
                    event_id = %id,
                    "Strategy ACK for unknown event_id — already released or never registered"
                );
                continue;
            };
            if state.queue[idx].sent {
                if let Some(event) = state.queue.remove(idx) {
                    released.push(event.id);
                }
            } else {
                state.queue[idx].acked = true;
                debug!(
                    event_id = %id,
                    "Strategy ACK arrived before delivery confirmation — releasing on mark_sent"
                );
            }
        }

        if !released.is_empty() {
            let in_flight = state.queue.len();
            drop(state);

            if self.engine_ack_tx.send(released).is_err() {
                warn!("Engine ACK channel closed, dropping ACK");
            } else {
                info!(in_flight, "Forwarded strategy ACK to engine");
            }
        }
    }

    /// Immediately ACK an event to the engine (bypasses pending tracking).
    ///
    /// Called when an event does NOT match the subscription filter. The event
    /// is ACK'd immediately without waiting for strategy acknowledgment.
    pub fn immediate_ack(&self, event_id: String) {
        debug!(event_id = %event_id, "Immediate ACK (not subscribed)");

        if self.engine_ack_tx.send(vec![event_id]).is_err() {
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
    async fn handle_strategy_ack(&self, event_ids: &[String]) {
        self.handle_strategy_ack(event_ids).await;
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

    fn acked(ids: &[&str]) -> Vec<String> {
        ids.iter().map(ToString::to_string).collect()
    }

    #[tokio::test]
    async fn test_register_pending_tracks_event() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let bridge = TektiiAckBridge::new(tx);

        bridge.register_pending("evt-0".to_string()).await;
        bridge.register_pending("evt-1".to_string()).await;

        assert_eq!(bridge.pending_count().await, 2);
    }

    /// Strict-ID release: each ACK frees exactly the event it names — never
    /// a positional head, never everything delivered.
    #[tokio::test]
    async fn test_ack_releases_exactly_the_named_event() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let bridge = TektiiAckBridge::new(tx);

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

        // The ACK names evt-1 — evt-0 (the queue head) must not leak out.
        bridge.handle_strategy_ack(&acked(&["evt-1"])).await;

        assert_eq!(
            rx.recv().await.expect("Should receive ACK"),
            vec!["evt-1".to_string()]
        );
        assert!(
            rx.try_recv().is_err(),
            "only the named event may be released"
        );
        assert_eq!(bridge.pending_count().await, 2);

        bridge.handle_strategy_ack(&acked(&["evt-0"])).await;
        assert_eq!(
            rx.recv().await.expect("second ACK"),
            vec!["evt-0".to_string()]
        );
        bridge.handle_strategy_ack(&acked(&["evt-2"])).await;
        assert_eq!(
            rx.recv().await.expect("third ACK"),
            vec!["evt-2".to_string()]
        );
        assert_eq!(bridge.pending_count().await, 0);
    }

    /// Fill-batch regression guard: an engine fill broadcasts a multi-event batch
    /// (order, trade, account, position) and waits for ALL ids before
    /// advancing sim time. The engine's batch wait completes only after the
    /// strategy has acked every event in the batch — each per-event ACK
    /// names its event, so a stale ACK can never release the whole batch or
    /// a later candle the strategy hasn't seen.
    #[tokio::test]
    async fn test_fill_batch_releases_only_named_events() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let bridge = TektiiAckBridge::new(tx);

        let ids = ["order", "trade", "account", "position", "candle"];
        for id in ids {
            bridge.register_pending(id.to_string()).await;
        }
        bridge
            .mark_sent(ids.iter().map(ToString::to_string).collect())
            .await;

        // The strategy consumes + acks one event at a time, naming each.
        // The candle is only released by its own ACK.
        for expected in ids {
            bridge.handle_strategy_ack(&acked(&[expected])).await;
            assert_eq!(
                rx.recv().await.expect("ACK forwarded"),
                vec![expected.to_string()],
                "each strategy ACK must release exactly the event it names"
            );
        }
        assert_eq!(bridge.pending_count().await, 0);
    }

    /// A delivery loss must not wedge the queue: an ACK naming a delivered
    /// event releases it even when an older, never-delivered event is still
    /// pending. The engine matches ACKs by exact id, so out-of-order release
    /// is safe — the lost event costs one ACK-window timeout, not the run.
    #[tokio::test]
    async fn test_ack_releases_named_event_past_undelivered_older_event() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let bridge = TektiiAckBridge::new(tx);

        bridge.register_pending("evt-a".to_string()).await;
        bridge.register_pending("evt-b".to_string()).await;
        // Only the later event was actually delivered.
        bridge.mark_sent(vec!["evt-b".to_string()]).await;

        bridge.handle_strategy_ack(&acked(&["evt-b"])).await;
        assert_eq!(
            rx.recv().await.expect("evt-b ACK"),
            vec!["evt-b".to_string()]
        );
        assert_eq!(
            bridge.pending_count().await,
            1,
            "evt-a stays pending until delivered and acked"
        );
    }

    /// An ACK naming an id the bridge never registered releases nothing.
    #[tokio::test]
    async fn test_ack_with_unknown_id_is_noop() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let bridge = TektiiAckBridge::new(tx);

        bridge.register_pending("evt-0".to_string()).await;
        bridge.mark_sent(vec!["evt-0".to_string()]).await;

        bridge.handle_strategy_ack(&acked(&["evt-bogus"])).await;

        assert!(
            rx.try_recv().is_err(),
            "unknown id must not release the pending event"
        );
        assert_eq!(bridge.pending_count().await, 1);
    }

    /// An empty ACK payload (gateway-local events carry no engine id)
    /// releases nothing, even with delivered events pending.
    #[tokio::test]
    async fn test_empty_ack_is_noop() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let bridge = TektiiAckBridge::new(tx);

        bridge.register_pending("evt-0".to_string()).await;
        bridge.mark_sent(vec!["evt-0".to_string()]).await;

        bridge.handle_strategy_ack(&[]).await;

        assert!(
            rx.try_recv().is_err(),
            "empty ACK must not release anything"
        );
        assert_eq!(bridge.pending_count().await, 1);
    }

    /// Duplicate ACKs are idempotent — the id is gone after the first
    /// release, so the second finds nothing.
    #[tokio::test]
    async fn test_duplicate_ack_releases_once() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let bridge = TektiiAckBridge::new(tx);

        bridge.register_pending("evt-0".to_string()).await;
        bridge.mark_sent(vec!["evt-0".to_string()]).await;

        bridge.handle_strategy_ack(&acked(&["evt-0"])).await;
        assert_eq!(
            rx.recv().await.expect("first ACK"),
            vec!["evt-0".to_string()]
        );

        bridge.handle_strategy_ack(&acked(&["evt-0"])).await;
        assert!(rx.try_recv().is_err(), "duplicate ACK must release nothing");
        assert_eq!(bridge.pending_count().await, 0);
    }

    /// A single ACK frame naming several ids (batch ACK) releases all of
    /// them in one engine frame.
    #[tokio::test]
    async fn test_multi_id_ack_releases_all_named_events() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let bridge = TektiiAckBridge::new(tx);

        bridge.register_pending("evt-0".to_string()).await;
        bridge.register_pending("evt-1".to_string()).await;
        bridge
            .mark_sent(vec!["evt-0".to_string(), "evt-1".to_string()])
            .await;

        bridge
            .handle_strategy_ack(&acked(&["evt-0", "evt-1"]))
            .await;

        assert_eq!(
            rx.recv().await.expect("batch ACK"),
            vec!["evt-0".to_string(), "evt-1".to_string()]
        );
        assert_eq!(bridge.pending_count().await, 0);
    }

    #[tokio::test]
    async fn test_immediate_ack_bypasses_pending() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let bridge = TektiiAckBridge::new(tx);

        bridge.register_pending("evt-0".to_string()).await;

        bridge.immediate_ack("evt-1".to_string());

        let acked = rx.recv().await.expect("Should receive immediate ACK");
        assert_eq!(acked, vec!["evt-1".to_string()]);

        assert_eq!(bridge.pending_count().await, 1);
    }

    /// Delivery-confirmation race guard: a strategy ACK naming an event still in
    /// flight (`sent = false`) must not release it — the engine must never
    /// see an ACK for an event whose delivery the registry has not
    /// confirmed.
    #[tokio::test]
    async fn test_ack_for_undelivered_event_is_held_until_mark_sent() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let bridge = TektiiAckBridge::new(tx);

        // Engine emits an event, registered pending at WS-read time.
        bridge.register_pending("evt-1".to_string()).await;

        // Race: strategy ACK arrives before registry's `mark_sent` fires
        // (the sub-µs window between send_to and mark_sent).
        bridge.handle_strategy_ack(&acked(&["evt-1"])).await;

        assert!(
            rx.try_recv().is_err(),
            "No engine ACK should fire while the event is in flight"
        );

        // Registry now confirms delivery — the held ACK fires immediately.
        bridge.mark_sent(vec!["evt-1".to_string()]).await;

        assert_eq!(
            rx.recv().await.expect("held ACK fires on mark_sent"),
            vec!["evt-1".to_string()]
        );
        assert_eq!(bridge.pending_count().await, 0);
    }

    /// The held ACK is per-event: confirming delivery of an event the
    /// strategy never acked must not auto-fire anything.
    #[tokio::test]
    async fn test_mark_sent_without_prior_ack_releases_nothing() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let bridge = TektiiAckBridge::new(tx);

        bridge.register_pending("evt-1".to_string()).await;
        bridge.register_pending("evt-2".to_string()).await;

        // Strategy acks evt-1 while both are in flight — held on evt-1.
        bridge.handle_strategy_ack(&acked(&["evt-1"])).await;
        assert!(rx.try_recv().is_err());

        // Delivery confirmation for evt-1 fires its held ACK.
        bridge.mark_sent(vec!["evt-1".to_string()]).await;
        assert_eq!(
            rx.recv().await.expect("held ACK"),
            vec!["evt-1".to_string()]
        );

        // evt-2 was never acked — its mark_sent must not release anything.
        bridge.mark_sent(vec!["evt-2".to_string()]).await;
        assert!(
            rx.try_recv().is_err(),
            "mark_sent must not release an event the strategy never acked"
        );
        assert_eq!(bridge.pending_count().await, 1);

        bridge.handle_strategy_ack(&acked(&["evt-2"])).await;
        assert_eq!(
            rx.recv().await.expect("evt-2 ACK"),
            vec!["evt-2".to_string()]
        );
    }

    /// Two events both ACKed before delivery confirmation, then confirmed by
    /// a single `mark_sent` batch — both release together in one engine
    /// frame.
    #[tokio::test]
    async fn test_mark_sent_batch_releases_all_held_acks() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let bridge = TektiiAckBridge::new(tx);

        bridge.register_pending("evt-1".to_string()).await;
        bridge.register_pending("evt-2".to_string()).await;

        bridge.handle_strategy_ack(&acked(&["evt-1"])).await;
        bridge.handle_strategy_ack(&acked(&["evt-2"])).await;
        assert!(rx.try_recv().is_err());

        bridge
            .mark_sent(vec!["evt-1".to_string(), "evt-2".to_string()])
            .await;

        assert_eq!(
            rx.recv().await.expect("batch release"),
            vec!["evt-1".to_string(), "evt-2".to_string()]
        );
        assert_eq!(bridge.pending_count().await, 0);
    }

    /// Releasing by id from the middle of a deeper queue, out of order, must
    /// never remove a neighbouring entry (index-shift regression guard).
    #[tokio::test]
    async fn test_out_of_order_release_removes_only_the_named_events() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let bridge = TektiiAckBridge::new(tx);

        let ids = ["evt-1", "evt-2", "evt-3", "evt-4"];
        for id in ids {
            bridge.register_pending(id.to_string()).await;
        }
        bridge
            .mark_sent(ids.iter().map(ToString::to_string).collect())
            .await;

        for id in ["evt-2", "evt-4", "evt-1", "evt-3"] {
            bridge.handle_strategy_ack(&acked(&[id])).await;
            assert_eq!(
                rx.recv().await.expect("release"),
                vec![id.to_string()],
                "out-of-order release must free exactly the named event"
            );
        }
        assert_eq!(bridge.pending_count().await, 0);
    }

    /// Repeated ACKs naming an in-flight event collapse to one release.
    #[tokio::test]
    async fn test_repeated_acks_for_in_flight_event_release_once() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let bridge = TektiiAckBridge::new(tx);

        bridge.register_pending("evt-1".to_string()).await;

        bridge.handle_strategy_ack(&acked(&["evt-1"])).await;
        bridge.handle_strategy_ack(&acked(&["evt-1"])).await;
        bridge.handle_strategy_ack(&acked(&["evt-1"])).await;
        assert!(rx.try_recv().is_err());

        bridge.mark_sent(vec!["evt-1".to_string()]).await;
        assert_eq!(
            rx.recv().await.expect("Should fire once"),
            vec!["evt-1".to_string()]
        );
        assert!(rx.try_recv().is_err());
    }

    /// `mark_sent` for an unknown event_id is silently ignored (no panic).
    /// Defensive against ordering edge cases (e.g., late mark_sent after a
    /// drain, or duplicate mark_sent).
    #[tokio::test]
    async fn test_mark_sent_unknown_event_id_is_ignored() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let bridge = TektiiAckBridge::new(tx);

        bridge.mark_sent(vec!["unknown".to_string()]).await;

        assert_eq!(bridge.pending_count().await, 0);
    }
}
