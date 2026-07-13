//! Acknowledgment tracking for WebSocket events
//!
//! The `AckManager` is responsible for tracking pending event acknowledgments
//! and waiting for strategies to confirm they've processed events.
//!
//! This is critical for simulation where the engine must wait for strategies
//! to process events before advancing time. For live trading, acknowledgments
//! may be tracked but don't block event streaming.

// Allow dead code - these are public API types for future use by external consumers
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, warn};

use async_trait::async_trait;

use super::error::WebSocketError;
use super::messages::{EventAckMessage, WsMessage};

/// Manages acknowledgment tracking for WebSocket events
///
/// The `AckManager` maintains a registry of pending acknowledgments and provides
/// utilities for waiting on strategy confirmation that events have been processed.
///
/// # Usage Patterns
///
/// ## Synchronous Mode
/// ```ignore
/// // Register pending events
/// ack_manager.register_pending("conn-123", vec!["evt-1", "evt-2"]).await;
///
/// // Wait for acknowledgment before continuing
/// match ack_manager.wait_for_ack(events, Duration::from_secs(5)).await {
///     Ok(ack) => info!("Strategy acknowledged"),
///     Err(e) => warn!("ACK timeout: {}", e),
/// }
/// ```
///
/// ## Live Mode (Asynchronous)
/// ```ignore
/// // Track acknowledgments but don't block
/// ack_manager.register_pending("conn-123", vec!["evt-1"]).await;
/// // Continue streaming without waiting
/// ```
pub struct AckManager {
    /// Pending acknowledgments: `connection_id` -> list of event correlation IDs
    pending_acks: Arc<RwLock<HashMap<String, Vec<String>>>>,
    /// Channel for receiving acknowledgment messages
    ack_sender: mpsc::UnboundedSender<EventAckMessage>,
    /// Channel receiver (stored for cloning to waiters)
    ack_receiver: Arc<RwLock<Option<mpsc::UnboundedReceiver<EventAckMessage>>>>,
}

impl AckManager {
    /// Create a new `AckManager`
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            pending_acks: Arc::new(RwLock::new(HashMap::new())),
            ack_sender: tx,
            ack_receiver: Arc::new(RwLock::new(Some(rx))),
        }
    }

    /// Get a sender for acknowledgment messages
    ///
    /// This sender should be used by message handlers to forward incoming
    /// `EventAck` messages to the manager.
    pub fn get_sender(&self) -> mpsc::UnboundedSender<EventAckMessage> {
        self.ack_sender.clone()
    }

    /// Register events as pending acknowledgment
    ///
    /// # Arguments
    /// * `conn_id` - Connection ID that should send the acknowledgment
    /// * `event_ids` - List of event correlation IDs to track
    ///
    /// # Example
    /// ```ignore
    /// ack_manager.register_pending(
    ///     "conn-abc-123",
    ///     vec!["evt-1".to_string(), "evt-2".to_string()]
    /// ).await;
    /// ```
    pub async fn register_pending(&self, conn_id: String, event_ids: Vec<String>) {
        let mut pending = self.pending_acks.write().await;
        debug!(
            "Registering {} pending ACKs for connection {}",
            event_ids.len(),
            conn_id
        );
        pending.insert(conn_id, event_ids);
    }

    /// Wait for acknowledgment of specific events with timeout
    ///
    /// This method blocks until an acknowledgment is received that matches
    /// the expected events, or until the timeout expires.
    ///
    /// # Arguments
    /// * `expected_event_ids` - List of event IDs we're waiting for
    /// * `timeout` - Maximum time to wait for acknowledgment
    ///
    /// # Returns
    /// * `Ok(EventAckMessage)` - Acknowledgment received
    /// * `Err(WebSocketError::AckTimeout)` - Timeout expired
    /// * `Err(WebSocketError::InvalidAck)` - ACK doesn't match expected events
    ///
    /// # Example
    /// ```ignore
    /// let event_ids = vec!["evt-1".to_string(), "evt-2".to_string()];
    /// match ack_manager.wait_for_ack(event_ids, Duration::from_secs(5)).await {
    ///     Ok(ack) => info!("Received ACK for {} events", ack.events_processed.len()),
    ///     Err(e) => warn!("ACK error: {}", e),
    /// }
    /// ```
    pub async fn wait_for_ack(
        &self,
        expected_event_ids: Vec<String>,
        timeout: Duration,
    ) -> Result<EventAckMessage, WebSocketError> {
        debug!(
            "Waiting for ACK of {} events (timeout: {:?})",
            expected_event_ids.len(),
            timeout
        );

        let mut receiver_guard = self.ack_receiver.write().await;
        let mut receiver = receiver_guard
            .take()
            .ok_or_else(|| WebSocketError::Other("ACK receiver already taken".to_string()))?;

        drop(receiver_guard); // Release lock before waiting

        let result = tokio::select! {
            Some(ack) = receiver.recv() => {
                debug!("Received ACK for {} events", ack.events_processed.len());

                let all_matched = expected_event_ids.iter().all(|expected_id| {
                    ack.events_processed.contains(expected_id)
                });

                if all_matched {
                    self.mark_acknowledged_internal(&ack.events_processed).await;
                    Ok(ack)
                } else {
                    warn!(
                        "ACK mismatch: expected {:?}, got {:?}",
                        expected_event_ids, ack.events_processed
                    );
                    Err(WebSocketError::InvalidAck(
                        "ACK doesn't match expected events".to_string()
                    ))
                }
            }
            () = tokio::time::sleep(timeout) => {
                warn!("ACK timeout after {:?}", timeout);
                Err(WebSocketError::AckTimeout(format!(
                    "No ACK received within {timeout:?}"
                )))
            }
        };

        let mut receiver_guard = self.ack_receiver.write().await;
        *receiver_guard = Some(receiver);

        result
    }

    /// Mark events as acknowledged (external API)
    ///
    /// This is called when an `EventAck` message is received via a different path
    /// than `wait_for_ack` (e.g., in live trading mode where we don't block).
    ///
    /// # Arguments
    /// * `ack` - The acknowledgment message received
    pub async fn mark_acknowledged(&self, ack: &EventAckMessage) -> Result<(), WebSocketError> {
        debug!(
            "Marking {} events as acknowledged",
            ack.events_processed.len()
        );
        self.mark_acknowledged_internal(&ack.events_processed).await;

        // Forward to any waiters
        if let Err(e) = self.ack_sender.send(ack.clone()) {
            warn!("Failed to forward ACK to waiters: {}", e);
        }

        Ok(())
    }

    async fn mark_acknowledged_internal(&self, event_ids: &[String]) {
        let mut pending = self.pending_acks.write().await;

        pending.retain(|conn_id, pending_event_ids| {
            let matched = event_ids
                .iter()
                .all(|ack_id| pending_event_ids.contains(ack_id));

            if matched {
                debug!("Cleared pending ACKs for connection {}", conn_id);
                false
            } else {
                true
            }
        });
    }

    /// Get count of pending acknowledgments
    ///
    /// Useful for monitoring and debugging.
    pub async fn pending_count(&self) -> usize {
        let pending = self.pending_acks.read().await;
        pending.values().map(Vec::len).sum()
    }

    /// Clear all pending acknowledgments for a connection
    ///
    /// Called when a connection is closed to prevent memory leaks.
    ///
    /// # Arguments
    /// * `conn_id` - Connection ID to clear
    pub async fn clear_connection(&self, conn_id: &str) {
        let mut pending = self.pending_acks.write().await;
        if let Some(removed) = pending.remove(conn_id) {
            debug!(
                "Cleared {} pending ACKs for disconnected connection {}",
                removed.len(),
                conn_id
            );
        }
    }

    /// Extract correlation IDs from a list of WebSocket messages
    ///
    /// Helper method to get event IDs from messages for ACK tracking.
    pub fn extract_correlation_ids(messages: &[WsMessage]) -> Vec<String> {
        messages
            .iter()
            .filter_map(|msg| {
                // WsMessage carries no correlation id, so order/position events key
                // off their own id; market data and control events need no ACK.
                match msg {
                    WsMessage::Order { order, .. } => Some(order.id.clone()),
                    WsMessage::Position { position, .. } => Some(position.id.clone()),
                    _ => None,
                }
            })
            .collect()
    }
}

impl Default for AckManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Bridge trait for acknowledgment handling between providers and strategies.
///
/// This trait is implemented by specific adapters (e.g., the Tektii adapter)
/// that need custom ACK behavior. `ProviderRegistry` holds `Arc<dyn AckBridge>`.
///
/// # Two-phase pending lifecycle
///
/// Pending events are tracked in two phases to avoid a delivery race:
///
/// 1. `register_pending` is called at WS-read time, BEFORE the event has been
///    delivered to the strategy WebSocket. The event is tracked but NOT yet
///    eligible to be ACK'd back to the engine.
/// 2. `mark_sent` is called by the registry's broadcast loop after
///    `connection_manager.send_to` returns successfully. Only events that
///    have been marked sent are eligible for release by a strategy ACK.
///
/// # Strict-ID release
///
/// `handle_strategy_ack` releases exactly the events the ACK names: the
/// gateway echoes the engine `event_id` on every outbound engine frame, the
/// SDK returns it in `events_processed`, and the bridge releases those ids —
/// never by queue position. An ACK naming an unknown id releases nothing; an
/// empty ACK (gateway-local events carry no engine id) releases nothing.
/// Positional release required the bridge's queue to mirror the engine's
/// send order perfectly across every delivery failure — one lost event or
/// stray ACK offset it permanently, surfacing far away as over-popped sim
/// time (0-trade runs) or a starved-FIFO halt. Naming the id makes each
/// release self-describing and delivery losses self-healing. Events still
/// in flight (registered but not yet sent) have their ACK held until
/// `mark_sent` confirms delivery.
#[async_trait]
pub trait AckBridge: Send + Sync {
    /// Handle an ACK from a connected strategy, releasing exactly the named
    /// events (once delivered). Unknown ids and empty ACKs release nothing.
    async fn handle_strategy_ack(&self, event_ids: &[String]);

    /// Register events as pending acknowledgment (phase 1: WS read).
    async fn register_pending(&self, event_ids: Vec<String>);

    /// Mark events as actually delivered to the strategy WebSocket
    /// (phase 2: post `send_to`). Unknown event_ids are silently ignored.
    async fn mark_sent(&self, event_ids: Vec<String>);

    /// Immediately acknowledge events (bypass pending tracking).
    async fn immediate_ack(&self, event_ids: Vec<String>);
}
