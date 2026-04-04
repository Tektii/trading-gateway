//! WebSocket connection utilities
//!
//! This module provides core connection management primitives that can be used
//! by both the Tektii engine and live trading proxy implementations.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, mpsc};
use uuid::Uuid;

use super::messages::WsMessage;

/// Represents a single WebSocket connection
///
/// This struct contains the essential information for managing a WebSocket connection:
/// - Unique identifier
/// - Message sender channel
/// - Client address
/// - Last activity timestamp for heartbeat monitoring
#[derive(Debug)]
pub struct WsConnection {
    /// Connection unique identifier
    pub id: Uuid,

    /// Message sender channel
    pub sender: mpsc::Sender<WsMessage>,

    /// Client socket address
    pub addr: SocketAddr,

    /// Last activity timestamp for heartbeat monitoring
    pub last_activity: Arc<RwLock<Instant>>,

    /// Monotonic timestamp of when the connection was established
    pub connected_at: Instant,
}

impl WsConnection {
    /// Create a new WebSocket connection
    pub fn new(sender: mpsc::Sender<WsMessage>, addr: SocketAddr) -> Self {
        Self {
            id: Uuid::new_v4(),
            sender,
            addr,
            last_activity: Arc::new(RwLock::new(Instant::now())),
            connected_at: Instant::now(),
        }
    }

    /// Update last activity timestamp
    pub async fn update_activity(&self) {
        let mut last_activity = self.last_activity.write().await;
        *last_activity = Instant::now();
    }

    /// Check if connection is stale (no activity for timeout duration)
    pub async fn is_stale(&self, timeout: Duration) -> bool {
        let last_activity = self.last_activity.read().await;
        last_activity.elapsed() > timeout
    }

    /// Send a message to this connection.
    ///
    /// Uses `try_send` on the bounded channel. If the channel is full, the
    /// strategy is too slow to keep up and will be disconnected — returning
    /// `Err` signals the caller to clean up this connection. This is the
    /// industry-standard approach for trading feeds (silent message drops
    /// would leave the strategy trading on a stale world model).
    pub fn send(&self, message: WsMessage) -> Result<(), String> {
        match self.sender.try_send(message) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                metrics::counter!("gateway_ws_slow_consumer_disconnects").increment(1);
                tracing::warn!(
                    connection_id = %self.id,
                    "Client channel full — disconnecting slow consumer"
                );
                Err(format!(
                    "Connection {} channel full — slow consumer",
                    self.id
                ))
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                Err(format!("Connection {} channel closed", self.id))
            }
        }
    }
}

/// Basic WebSocket connection manager
///
/// This provides core connection management functionality that can be used
/// by both the Tektii engine and live trading proxy implementations.
#[derive(Debug, Clone)]
pub struct WsConnectionManager {
    connections: Arc<RwLock<HashMap<Uuid, Arc<WsConnection>>>>,
}

impl WsConnectionManager {
    /// Create a new connection manager
    pub fn new() -> Self {
        Self {
            connections: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Add a new connection
    ///
    /// # Returns
    ///
    /// The UUID of the added connection
    pub async fn add_connection(&self, conn: WsConnection) -> Uuid {
        let id = conn.id;
        {
            let mut connections = self.connections.write().await;
            connections.insert(id, Arc::new(conn));
        }
        metrics::gauge!("gateway_ws_connections_active").increment(1.0);
        tracing::info!(connection_id = %id, "WebSocket connection added");
        id
    }

    /// Remove a connection
    pub async fn remove_connection(&self, id: &Uuid) {
        let mut connections = self.connections.write().await;
        if connections.remove(id).is_some() {
            metrics::gauge!("gateway_ws_connections_active").decrement(1.0);
            tracing::info!(connection_id = %id, "WebSocket connection removed");
        }
    }

    /// Broadcast a message to all connected clients
    pub async fn broadcast(&self, message: WsMessage) {
        let connections = self.connections.read().await;
        let count = connections.len();

        for (id, conn) in &*connections {
            if let Err(e) = conn.send(message.clone()) {
                tracing::warn!(
                    connection_id = %id,
                    error = %e,
                    "Failed to send message to connection"
                );
            }
        }

        drop(connections);

        tracing::debug!(
            connection_count = count,
            "Broadcast message to all connections"
        );
    }

    /// Send a message to a specific connection
    ///
    /// # Returns
    ///
    /// `Ok(())` if message was sent successfully, `Err` if connection not found or send failed
    pub async fn send_to(&self, id: &Uuid, message: WsMessage) -> Result<(), String> {
        let connections = self.connections.read().await;
        connections.get(id).map_or_else(
            || Err(format!("Connection {id} not found")),
            |conn| {
                conn.send(message)
                    .map_err(|e| format!("Failed to send message: {e}"))
            },
        )
    }

    /// Get the number of active connections
    pub async fn connection_count(&self) -> usize {
        let connections = self.connections.read().await;
        connections.len()
    }

    /// Update last activity time for a connection
    pub async fn update_activity(&self, id: &Uuid) {
        let connections = self.connections.read().await;
        if let Some(conn) = connections.get(id) {
            conn.update_activity().await;
        }
    }

    /// Get the duration since a connection was established.
    pub async fn get_connection_duration(&self, id: &Uuid) -> Option<Duration> {
        let connections = self.connections.read().await;
        connections.get(id).map(|conn| conn.connected_at.elapsed())
    }

    /// Broadcast a graceful shutdown to all connected strategies.
    ///
    /// Sends a `Disconnecting` event, waits briefly for TCP delivery,
    /// then sends a WebSocket close frame (1001 Going Away).
    pub async fn broadcast_shutdown(&self) {
        use super::messages::ConnectionEventType;
        use chrono::Utc;

        let count = self.connection_count().await;
        if count == 0 {
            tracing::info!("No connected strategies — skipping shutdown broadcast");
            return;
        }

        tracing::info!(count, "Broadcasting shutdown to connected strategies");

        // 1. Send Disconnecting event
        self.broadcast(WsMessage::Connection {
            event: ConnectionEventType::Disconnecting,
            error: None,
            broker: None,
            gap_duration_ms: None,
            timestamp: Utc::now(),
        })
        .await;

        // 2. Brief pause for TCP delivery
        tokio::time::sleep(Duration::from_millis(500)).await;

        // 3. Send close frame
        self.broadcast(WsMessage::Close {
            code: 1001,
            reason: "Gateway shutting down".to_string(),
        })
        .await;
    }
}

impl Default for WsConnectionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a connection and return its receiver for inspecting sent messages.
    fn test_connection(addr: &str) -> (WsConnection, mpsc::Receiver<WsMessage>) {
        let (tx, rx) = mpsc::channel(1024);
        let conn = WsConnection {
            id: Uuid::new_v4(),
            sender: tx,
            addr: addr.parse().unwrap(),
            last_activity: Arc::new(tokio::sync::RwLock::new(std::time::Instant::now())),
            connected_at: Instant::now(),
        };
        (conn, rx)
    }

    #[tokio::test]
    async fn broadcast_shutdown_sends_disconnecting_then_close() {
        let mgr = WsConnectionManager::new();

        let (conn1, mut rx1) = test_connection("127.0.0.1:1001");
        let (conn2, mut rx2) = test_connection("127.0.0.1:1002");
        mgr.add_connection(conn1).await;
        mgr.add_connection(conn2).await;

        mgr.broadcast_shutdown().await;

        // Both receivers should get: Disconnecting, then Close
        for rx in [&mut rx1, &mut rx2] {
            let msg1 = rx.recv().await.expect("should receive Disconnecting");
            assert!(
                matches!(
                    msg1,
                    WsMessage::Connection {
                        event: super::super::messages::ConnectionEventType::Disconnecting,
                        ..
                    }
                ),
                "expected Disconnecting, got {msg1:?}"
            );

            let msg2 = rx.recv().await.expect("should receive Close");
            assert!(
                matches!(msg2, WsMessage::Close { code: 1001, .. }),
                "expected Close 1001, got {msg2:?}"
            );
        }
    }

    #[tokio::test]
    async fn broadcast_shutdown_with_no_connections_is_noop() {
        let mgr = WsConnectionManager::new();
        // Should not panic or error
        mgr.broadcast_shutdown().await;
        assert_eq!(mgr.connection_count().await, 0);
    }

    #[tokio::test]
    async fn get_connection_duration_returns_elapsed_time() {
        let mgr = WsConnectionManager::new();
        let (conn, _rx) = test_connection("127.0.0.1:5001");
        let id = conn.id;
        mgr.add_connection(conn).await;

        tokio::time::sleep(Duration::from_millis(10)).await;

        let duration = mgr.get_connection_duration(&id).await;
        assert!(duration.is_some(), "expected Some(duration)");
        assert!(
            duration.unwrap() >= Duration::from_millis(10),
            "expected at least 10ms, got {:?}",
            duration.unwrap()
        );
    }

    #[tokio::test]
    async fn get_connection_duration_returns_none_after_removal() {
        let mgr = WsConnectionManager::new();
        let (conn, _rx) = test_connection("127.0.0.1:5002");
        let id = conn.id;
        mgr.add_connection(conn).await;
        mgr.remove_connection(&id).await;

        assert!(mgr.get_connection_duration(&id).await.is_none());
    }

    #[tokio::test]
    async fn send_returns_error_when_channel_full() {
        let (tx, _rx) = mpsc::channel(2);
        let conn = WsConnection {
            id: Uuid::new_v4(),
            sender: tx,
            addr: "127.0.0.1:9999".parse().unwrap(),
            last_activity: Arc::new(RwLock::new(Instant::now())),
            connected_at: Instant::now(),
        };

        // Fill the buffer
        assert!(conn.send(WsMessage::ping()).is_ok());
        assert!(conn.send(WsMessage::ping()).is_ok());

        // Third send should fail — channel full, slow consumer disconnect
        let result = conn.send(WsMessage::ping());
        assert!(result.is_err());
        assert!(
            result.unwrap_err().contains("slow consumer"),
            "error should indicate slow consumer"
        );
    }

    #[tokio::test]
    async fn send_returns_error_when_channel_closed() {
        let (tx, rx) = mpsc::channel(2);
        drop(rx);
        let conn = WsConnection {
            id: Uuid::new_v4(),
            sender: tx,
            addr: "127.0.0.1:9999".parse().unwrap(),
            last_activity: Arc::new(RwLock::new(Instant::now())),
            connected_at: Instant::now(),
        };

        let result = conn.send(WsMessage::ping());
        assert!(result.is_err());
        assert!(
            result.unwrap_err().contains("closed"),
            "error should indicate channel closed"
        );
    }
}
