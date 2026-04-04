//! WebSocket server handler for streaming trading events to strategies.
//!
//! This module provides the WebSocket handler mounted at `/v1/ws` on the
//! main Axum router. Unlike the original proxy which ran a separate WS server,
//! the gateway uses a single-port design.
//!
//! Strategies connect via WebSocket and immediately start receiving market
//! events from all registered providers. No handshake or configuration
//! message is required.

#![allow(clippy::missing_errors_doc)]

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{ConnectInfo, State, WebSocketUpgrade};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::api::state::GatewayState;
use crate::websocket::connection::{WsConnection, WsConnectionManager};
use crate::websocket::messages::{DataStalenessEventType, WsMessage};
use crate::websocket::registry::ProviderRegistry;

/// Per-client outbound message buffer capacity.
///
/// When full, the strategy is disconnected (slow consumer protection).
/// 1024 messages provides ~10 seconds of headroom at typical market data rates.
const WS_CLIENT_CHANNEL_CAPACITY: usize = 1024;

/// WebSocket upgrade handler.
///
/// Mounted at `/v1/ws` on the main gateway router. Accepts WebSocket
/// upgrade requests and delegates to [`handle_socket`].
pub fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<GatewayState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    info!("WebSocket connection request from {}", addr);

    let connection_manager = state.ws_connection_manager().clone();
    let provider_registry = state.provider_registry().clone();

    ws.on_upgrade(move |socket| handle_socket(socket, connection_manager, provider_registry, addr))
}

/// Handle individual WebSocket connection.
///
/// Connection is immediately ready upon establishment - no handshake required.
/// Strategies should treat a successful WebSocket connection as "ready" and
/// immediately start receiving market events.
async fn handle_socket(
    socket: WebSocket,
    connection_manager: Arc<WsConnectionManager>,
    provider_registry: Arc<ProviderRegistry>,
    addr: SocketAddr,
) {
    let conn_id = Uuid::new_v4();
    info!("New WebSocket connection from {} (ID: {})", addr, conn_id);

    // Split socket into sender and receiver
    let (ws_sender, ws_receiver) = socket.split();

    // Create channel for outgoing messages
    let (tx, rx) = mpsc::channel::<WsMessage>(WS_CLIENT_CHANNEL_CAPACITY);

    // Create connection object
    let connection = WsConnection {
        id: conn_id,
        sender: tx,
        addr,
        last_activity: Arc::new(tokio::sync::RwLock::new(std::time::Instant::now())),
        connected_at: std::time::Instant::now(),
    };

    // Register connection with manager
    connection_manager.add_connection(connection).await;
    info!("Connection {} registered with manager", conn_id);

    // Register with provider registry to receive events
    provider_registry
        .register_strategy_connection(conn_id)
        .await;

    let platforms = provider_registry.connected_platforms().await;
    info!("Connection {} ready, providers = {:?}", conn_id, platforms);

    // Send current staleness state so the strategy knows if data is stale
    for platform in &platforms {
        if let Some(stale) = provider_registry.get_staleness_with_times(*platform).await
            && !stale.is_empty()
        {
            let symbols: Vec<String> = stale.iter().map(|(s, _)| s.clone()).collect();
            let earliest_stale = stale
                .iter()
                .map(|(_, t)| *t)
                .min()
                .expect("stale is non-empty");
            let msg = WsMessage::DataStaleness {
                event: DataStalenessEventType::Stale,
                symbols,
                stale_since: Some(earliest_stale),
                broker: Some(platform.to_string()),
                timestamp: chrono::Utc::now(),
            };
            let _ = connection_manager.send_to(&conn_id, msg).await;
        }
    }

    // Proceed directly to message handling
    handle_connection(
        conn_id,
        ws_sender,
        ws_receiver,
        rx,
        connection_manager,
        provider_registry,
        addr,
    )
    .await;
}

/// Handle an established WebSocket connection.
///
/// Manages bidirectional message flow between the strategy and providers.
async fn handle_connection(
    conn_id: Uuid,
    mut ws_sender: futures_util::stream::SplitSink<WebSocket, Message>,
    mut ws_receiver: futures_util::stream::SplitStream<WebSocket>,
    mut rx: mpsc::Receiver<WsMessage>,
    connection_manager: Arc<WsConnectionManager>,
    provider_registry: Arc<ProviderRegistry>,
    addr: SocketAddr,
) {
    debug!(
        "Starting message handling for configured connection {}",
        conn_id
    );

    // Spawn task to handle outgoing messages
    let manager_clone = connection_manager.clone();
    let send_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            // Intercept Close variant — send WebSocket close frame, not JSON
            if let WsMessage::Close { code, reason } = msg {
                debug!("Sending close frame to {} (code={})", conn_id, code);
                let close_frame = axum::extract::ws::CloseFrame {
                    code,
                    reason: reason.into(),
                };
                let _ = ws_sender.send(Message::Close(Some(close_frame))).await;
                break;
            }

            match serde_json::to_string(&msg) {
                Ok(json) => {
                    debug!("Sending message to {}: {:?}", conn_id, msg);
                    if let Err(e) = ws_sender.send(Message::Text(json.into())).await {
                        error!("Failed to send message to {}: {}", conn_id, e);
                        break;
                    }

                    manager_clone.update_activity(&conn_id).await;
                }
                Err(e) => {
                    error!("Failed to serialize message: {}", e);
                }
            }
        }
        debug!("Send task for connection {} completed", conn_id);
    });

    // Handle incoming messages from the strategy
    let manager_clone = connection_manager.clone();
    let registry_clone = provider_registry.clone();
    while let Some(result) = ws_receiver.next().await {
        match result {
            Ok(msg) => {
                if let Err(e) =
                    handle_client_message(msg, &conn_id, &manager_clone, &registry_clone).await
                {
                    warn!("Error handling message from {}: {}", conn_id, e);
                }

                manager_clone.update_activity(&conn_id).await;
            }
            Err(e) => {
                warn!("WebSocket error from {}: {}", conn_id, e);
                break;
            }
        }
    }

    // Strategy disconnect cleanup.
    //
    // Broker-side state (open positions, pending orders) is intentionally left
    // untouched. The gateway is transport infrastructure, not a risk manager.
    // Strategies are responsible for managing their own positions across
    // reconnections.

    // Compute duration before removing the connection (removal drops the object)
    let connection_duration_secs = manager_clone
        .get_connection_duration(&conn_id)
        .await
        .map(|d| d.as_secs());

    registry_clone.unregister_strategy_connection(conn_id).await;
    let remaining_strategies = registry_clone.connected_strategy_count().await;
    manager_clone.remove_connection(&conn_id).await;
    send_task.abort();

    info!(
        conn_id = %conn_id,
        remote_addr = %addr,
        duration_secs = ?connection_duration_secs,
        remaining_strategies = remaining_strategies,
        broker_state = "untouched",
        "Strategy disconnected"
    );
}

/// Handle incoming messages from client (strategy).
async fn handle_client_message(
    msg: Message,
    conn_id: &Uuid,
    _manager: &Arc<WsConnectionManager>,
    registry: &Arc<ProviderRegistry>,
) -> Result<(), String> {
    match msg {
        Message::Text(text) => {
            debug!("Received text message from {}: {}", conn_id, text);

            match serde_json::from_str::<WsMessage>(&text) {
                Ok(ws_msg) => {
                    handle_ws_message(ws_msg, conn_id, registry).await?;
                }
                Err(e) => {
                    warn!("Failed to parse message from {}: {}", conn_id, e);
                    return Err(format!("Invalid message format: {e}"));
                }
            }
        }
        Message::Binary(data) => {
            debug!(
                "Received binary message from {} ({} bytes)",
                conn_id,
                data.len()
            );
            warn!("Binary messages not supported, ignoring");
        }
        Message::Ping(_data) => {
            debug!("Received Ping from {}", conn_id);
            // Axum automatically responds to Ping with Pong
        }
        Message::Pong(_) => {
            debug!("Received Pong from {}", conn_id);
        }
        Message::Close(frame) => {
            info!("Received Close frame from {}: {:?}", conn_id, frame);
        }
    }

    Ok(())
}

/// Handle parsed WebSocket message.
async fn handle_ws_message(
    msg: WsMessage,
    conn_id: &Uuid,
    registry: &Arc<ProviderRegistry>,
) -> Result<(), String> {
    if let WsMessage::EventAck { .. } = msg {
        info!("Received EventAck from strategy {}", conn_id);
        registry.handle_strategy_ack().await;
    } else {
        warn!("Unexpected message type from client {}: {:?}", conn_id, msg);
    }
    Ok(())
}
