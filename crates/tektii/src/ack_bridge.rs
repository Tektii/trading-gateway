//! ACK bridge for coordinating strategy acknowledgments with the Tektii Engine.
//!
//! In simulation mode, the engine waits for ACKs before advancing simulation time.
//! This bridge tracks pending `event_ids` and forwards strategy ACKs to the engine.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use tektii_gateway_core::websocket::ack::AckBridge;
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, info, trace, warn};

/// Bridge for coordinating ACKs between strategy and engine in simulation mode.
///
/// The bridge tracks `event_ids` that need strategy acknowledgment before being
/// forwarded to the engine. When a strategy sends an ACK, all pending events
/// are drained and ACK'd to the engine (auto-correlation approach).
///
/// # Flow
///
/// 1. Engine sends event with `event_id` to gateway
/// 2. Gateway checks subscription filter:
///    - NOT subscribed: `immediate_ack()` → engine ACK sent immediately
///    - SUBSCRIBED: `register_pending()` → `event_id` tracked, event forwarded to strategy
/// 3. Strategy processes event and sends `EventAck`
/// 4. Gateway calls `handle_strategy_ack()` → drains all pending `event_ids`
/// 5. Engine receives ACK and advances simulation time
#[derive(Debug)]
pub struct TektiiAckBridge {
    /// Pending `event_ids` awaiting strategy ACK.
    pending_events: Arc<RwLock<HashSet<String>>>,

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
            pending_events: Arc::new(RwLock::new(HashSet::new())),
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
            pending_events: Arc::new(RwLock::new(HashSet::new())),
            engine_ack_tx,
        }
    }

    /// Register an event as pending strategy ACK.
    ///
    /// Called when an event matches the subscription filter and is forwarded
    /// to the strategy. The `event_id` will be ACK'd to the engine when the
    /// strategy sends any `EventAck` message.
    pub async fn register_pending(&self, event_id: String) {
        let mut pending = self.pending_events.write().await;
        pending.insert(event_id.clone());
        debug!(event_id = %event_id, pending_count = pending.len(), "Registered pending event");
    }

    /// Handle strategy ACK by draining all pending events and forwarding to engine.
    ///
    /// This implements the auto-correlation approach: ANY ACK from the strategy
    /// triggers ACK of ALL pending events to the engine. This simplifies the
    /// strategy implementation (no need to track engine `event_ids`).
    pub async fn handle_strategy_ack(&self) {
        let mut pending = self.pending_events.write().await;

        if pending.is_empty() {
            warn!("Strategy ACK received but no pending events to forward");
            return;
        }

        let event_ids: Vec<String> = pending.drain().collect();
        let count = event_ids.len();

        if self.engine_ack_tx.send(event_ids).is_err() {
            // Channel closed - engine writer task has stopped
            // This is expected during shutdown, no action needed
            warn!("Engine ACK channel closed, dropping ACKs");
        } else {
            info!(count, "Forwarded strategy ACK to engine");
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
        self.pending_events.read().await.len()
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

        // Register some pending events
        bridge.register_pending("evt-0".to_string()).await;
        bridge.register_pending("evt-1".to_string()).await;
        bridge.register_pending("evt-2".to_string()).await;

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

        // First batch
        bridge.register_pending("evt-0".to_string()).await;
        bridge.handle_strategy_ack().await;

        let batch1 = rx.recv().await.expect("Should receive first batch");
        assert_eq!(batch1.len(), 1);

        // Second batch
        bridge.register_pending("evt-1".to_string()).await;
        bridge.register_pending("evt-2".to_string()).await;
        bridge.handle_strategy_ack().await;

        let batch2 = rx.recv().await.expect("Should receive second batch");
        assert_eq!(batch2.len(), 2);

        // Pending should be empty
        assert_eq!(bridge.pending_count().await, 0);
    }
}
