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

        // Take the receiver to await on it
        let mut receiver_guard = self.ack_receiver.write().await;
        let mut receiver = receiver_guard
            .take()
            .ok_or_else(|| WebSocketError::Other("ACK receiver already taken".to_string()))?;

        drop(receiver_guard); // Release lock before waiting

        // Wait for ACK message or timeout
        let result = tokio::select! {
            Some(ack) = receiver.recv() => {
                debug!("Received ACK for {} events", ack.events_processed.len());

                // Verify ACK matches expected events
                let all_matched = expected_event_ids.iter().all(|expected_id| {
                    ack.events_processed.contains(expected_id)
                });

                if all_matched {
                    // Mark as acknowledged
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

        // Return the receiver
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

    /// Internal method to mark events as acknowledged
    async fn mark_acknowledged_internal(&self, event_ids: &[String]) {
        let mut pending = self.pending_acks.write().await;

        // Remove matching entries
        pending.retain(|conn_id, pending_event_ids| {
            // Check if this connection's pending events match the ACK
            let matched = event_ids
                .iter()
                .all(|ack_id| pending_event_ids.contains(ack_id));

            if matched {
                debug!("Cleared pending ACKs for connection {}", conn_id);
                false // Remove this entry
            } else {
                true // Keep this entry
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
                // Only extract IDs from event messages that require acknowledgment
                // Note: WsMessage types don't have correlation_ids.
                // For order events, we use the order ID.
                // For market data events, we use None (no ACK required for live trading).
                match msg {
                    WsMessage::Order { order, .. } => Some(order.id.clone()),
                    WsMessage::Position { position, .. } => Some(position.id.clone()),
                    // Market data and control messages don't require ACK
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
/// Pending events are tracked in two phases to avoid the race documented in
/// TEK-270:
///
/// 1. `register_pending` is called at WS-read time, BEFORE the event has been
///    delivered to the strategy WebSocket. The event is tracked but NOT yet
///    eligible to be ACK'd back to the engine.
/// 2. `mark_sent` is called by the registry's broadcast loop after
///    `connection_manager.send_to` returns successfully. Only events that
///    have been marked sent will be drained on the next strategy ACK.
///
/// `handle_strategy_ack` drains only events with `sent = true`. Events still
/// in flight (registered but not yet sent) remain pending and will be drained
/// by a later strategy ACK once they reach the strategy.
#[async_trait]
pub trait AckBridge: Send + Sync {
    /// Handle an ACK from a connected strategy. Drains only events that have
    /// been marked sent.
    async fn handle_strategy_ack(&self);

    /// Register events as pending acknowledgment (phase 1: WS read).
    async fn register_pending(&self, event_ids: Vec<String>);

    /// Mark events as actually delivered to the strategy WebSocket
    /// (phase 2: post `send_to`). Unknown event_ids are silently ignored.
    async fn mark_sent(&self, event_ids: Vec<String>);

    /// Immediately acknowledge events (bypass pending tracking).
    async fn immediate_ack(&self, event_ids: Vec<String>);
}
