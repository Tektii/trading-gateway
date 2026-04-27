//! WebSocket provider for the Tektii Engine.
//!
//! Connects to the engine's WebSocket endpoint and routes events through the `EventRouter`.
//!
//! **ACK rule**: every engine event that produces a downstream `WsMessage` registers
//! pending and waits for strategy ACK (engine paces simulation on every emitted
//! event). Engine messages that don't produce a downstream broadcast (`Error`,
//! `Pong`) are immediately ACK'd to the engine — registering them would block
//! simulation time on an event the strategy will never see.
//!
//! Filtering is upstream-only: the engine validates `SUBSCRIPTIONS` at startup
//! against its instrument catalog and only emits events for the resolved set, so
//! the gateway forwards every event it receives. `filters_events_upstream()`
//! returns `true` to tell the `ProviderRegistry` to skip its own filter.
//!
//! This provider implements the [`WebSocketProvider`] trait, allowing it to be used
//! through the same `connect_from_config()` path as live providers (Alpaca, Binance).

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use tektii_protocol::websocket::{ClientMessage, ServerMessage};
use tokio::sync::{RwLock, mpsc};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use tektii_gateway_core::events::router::EventRouter;
use tektii_gateway_core::models::{Bar, Timeframe, TradingPlatform};
use tektii_gateway_core::websocket::error::WebSocketError;
use tektii_gateway_core::websocket::messages::{
    AccountEventType, EventAckMessage, PositionEventType, TradeEventType, WsMessage,
};
use tektii_gateway_core::websocket::provider::{EventStream, ProviderConfig, WebSocketProvider};

use crate::ack_bridge::TektiiAckBridge;
use crate::conversions;

/// WebSocket provider for connecting to the Tektii Engine.
///
/// This provider:
/// 1. Connects to the engine's WebSocket endpoint
/// 2. Receives `ServerMessage` events (Order, Trade, Position, Account, Candle)
/// 3. Translates engine types to gateway types using `conversions`
/// 4. Routes events through `EventRouter` for state management and broadcasting
/// 5. **Implements proper ACK handling** for simulation time synchronization
pub struct TektiiWebSocketProvider {
    /// WebSocket URL for the engine (e.g., "<ws://localhost:8081>")
    ws_url: String,
    /// Event router for processing and broadcasting events
    event_router: Arc<EventRouter>,
    /// Platform identifier
    platform: TradingPlatform,
    /// ACK bridge for coordinating strategy ACKs with the engine.
    /// Created during `connect()` and used by `handle_ack()`.
    ack_bridge: Arc<RwLock<Option<Arc<TektiiAckBridge>>>>,
    /// Cancellation token for graceful shutdown of background tasks.
    cancel_token: CancellationToken,
}

impl TektiiWebSocketProvider {
    /// Create a new `TektiiWebSocketProvider`.
    #[must_use]
    pub fn new(ws_url: String, event_router: Arc<EventRouter>, platform: TradingPlatform) -> Self {
        Self {
            ws_url,
            event_router,
            platform,
            ack_bridge: Arc::new(RwLock::new(None)),
            cancel_token: CancellationToken::new(),
        }
    }

    /// Get the ACK bridge for this provider.
    ///
    /// Returns `None` if `connect()` has not been called yet.
    pub async fn ack_bridge(&self) -> Option<Arc<TektiiAckBridge>> {
        self.ack_bridge.read().await.clone()
    }
}

/// Parse a timeframe string to Timeframe.
///
/// Falls back to the default timeframe (1h) if parsing fails, with a warning log.
fn parse_timeframe(s: &str) -> Timeframe {
    if let Ok(tf) = s.parse() {
        tf
    } else {
        warn!(
            timeframe = %s,
            "Unknown timeframe from engine, defaulting to 1h"
        );
        Timeframe::default()
    }
}

/// Connect to a Tektii Engine WebSocket endpoint with exponential backoff.
///
/// Retries connection-refused errors up to 60 times with delays from 100ms to 2s.
async fn connect_engine_with_backoff(
    ws_endpoint: &str,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    String,
> {
    const MAX_RETRIES: u32 = 60;
    const INITIAL_DELAY_MS: u64 = 100;
    const MAX_DELAY_MS: u64 = 2000;

    let mut attempt = 0;
    let mut delay_ms = INITIAL_DELAY_MS;

    loop {
        attempt += 1;
        match connect_async(ws_endpoint).await {
            Ok((stream, _response)) => {
                if attempt > 1 {
                    info!(attempt, "Connected to engine after retry");
                }
                return Ok(stream);
            }
            Err(e) => {
                let is_connection_refused = e.to_string().contains("Connection refused")
                    || e.to_string().contains("ConnectionRefused");

                if attempt >= MAX_RETRIES || !is_connection_refused {
                    return Err(format!("Failed after {attempt} attempts: {e}"));
                }

                debug!(
                    attempt,
                    delay_ms, "Engine not ready, retrying connection..."
                );
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                delay_ms = (delay_ms * 2).min(MAX_DELAY_MS);
            }
        }
    }
}

/// Errors that can occur in the Tektii WebSocket provider.
#[derive(Debug, thiserror::Error)]
pub enum TektiiWsError {
    #[error("Failed to connect to engine: {0}")]
    ConnectionFailed(String),

    #[error("Connection error: {0}")]
    ConnectionError(String),

    #[error("Failed to send message: {0}")]
    SendFailed(String),
}

// =============================================================================
// WebSocketProvider Trait Implementation
// =============================================================================

#[async_trait]
impl WebSocketProvider for TektiiWebSocketProvider {
    async fn connect(&self, _config: ProviderConfig) -> Result<EventStream, WebSocketError> {
        let (ack_bridge, engine_ack_rx) = TektiiAckBridge::create();

        // Store the ACK bridge so it can be retrieved later
        {
            let mut bridge_guard = self.ack_bridge.write().await;
            *bridge_guard = Some(ack_bridge.clone());
        }

        let ws_endpoint = format!("{}/ws", self.ws_url);
        info!(url = %ws_endpoint, "Connecting to Tektii Engine WebSocket");

        let ws_stream = connect_engine_with_backoff(&ws_endpoint)
            .await
            .map_err(WebSocketError::ConnectionFailed)?;

        info!("Connected to Tektii Engine WebSocket");

        let (mut write, read) = ws_stream.split();

        // Create event stream channel for broadcasting to strategies
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        // Spawn engine ACK writer task
        let engine_writer_task = {
            let mut engine_ack_rx = engine_ack_rx;
            tokio::spawn(async move {
                while let Some(event_ids) = engine_ack_rx.recv().await {
                    let ack = ClientMessage::event_ack(event_ids.clone());

                    let ack_json = match serde_json::to_string(&ack) {
                        Ok(json) => json,
                        Err(e) => {
                            error!(
                                error = %e,
                                "Failed to serialize EventAck - this indicates a bug"
                            );
                            continue;
                        }
                    };

                    if let Err(e) = write.send(Message::Text(ack_json.into())).await {
                        error!(error = %e, "Failed to send EventAck to engine");
                        break;
                    }
                    info!(count = event_ids.len(), "Sent EventAck to engine");
                }
                info!("Engine ACK writer task completed");
            })
        };

        // Spawn message processor task
        spawn_engine_message_processor(
            read,
            EngineMessageProcessorContext {
                event_tx,
                event_router: self.event_router.clone(),
                platform: self.platform,
                cancel_token: self.cancel_token.clone(),
                ack_bridge: ack_bridge.clone(),
                engine_writer_task,
            },
        );

        Ok(event_rx)
    }

    async fn subscribe(
        &self,
        _symbols: Vec<String>,
        _event_types: Vec<String>,
    ) -> Result<(), WebSocketError> {
        debug!("Tektii provider does not support dynamic subscriptions");
        Ok(())
    }

    async fn unsubscribe(&self, _symbols: Vec<String>) -> Result<(), WebSocketError> {
        debug!("Tektii provider does not support dynamic unsubscriptions");
        Ok(())
    }

    /// Handle incoming acknowledgment from strategy.
    ///
    /// **CRITICAL**: Unlike live providers where ACKs are informational only,
    /// Tektii ACKs are essential for engine time synchronization.
    async fn handle_ack(&self, ack: EventAckMessage) -> Result<(), WebSocketError> {
        let bridge = self.ack_bridge.read().await;

        if let Some(ref b) = *bridge {
            debug!(
                events_processed = ack.events_processed.len(),
                "Forwarding strategy ACK to engine"
            );
            b.handle_strategy_ack().await;
        } else {
            warn!("handle_ack called before connect() - ACK bridge not initialized");
        }

        Ok(())
    }

    async fn disconnect(&self) -> Result<(), WebSocketError> {
        info!("Disconnecting Tektii WebSocket provider");
        self.cancel_token.cancel();

        {
            let mut bridge_guard = self.ack_bridge.write().await;
            *bridge_guard = None;
        }

        Ok(())
    }

    async fn reconnect(&self) -> Result<EventStream, WebSocketError> {
        Err(WebSocketError::ProviderError(
            "Tektii engine provider does not support reconnection".to_string(),
        ))
    }

    fn supports_reconnection(&self) -> bool {
        false
    }

    fn filters_events_upstream(&self) -> bool {
        // Engine validates `SUBSCRIPTIONS` at startup and only emits events for
        // the resolved instrument/timeframe set; the gateway must forward every
        // event without re-applying its own (weaker-pattern-semantics) filter.
        true
    }
}

/// Context for the engine message processor background task.
struct EngineMessageProcessorContext {
    event_tx: mpsc::UnboundedSender<WsMessage>,
    event_router: Arc<EventRouter>,
    platform: TradingPlatform,
    cancel_token: CancellationToken,
    ack_bridge: Arc<TektiiAckBridge>,
    engine_writer_task: tokio::task::JoinHandle<()>,
}

/// Spawn the background task that reads engine WebSocket messages, routes them
/// through the `EventRouter`, and forwards them to the strategy event stream.
fn spawn_engine_message_processor(
    mut read: futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
    ctx: EngineMessageProcessorContext,
) {
    let EngineMessageProcessorContext {
        event_tx,
        event_router,
        platform,
        cancel_token,
        ack_bridge,
        engine_writer_task,
    } = ctx;

    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = cancel_token.cancelled() => {
                    info!("Tektii WebSocket provider cancelled");
                    break;
                }
                msg_result = read.next() => {
                    match msg_result {
                        Some(Ok(Message::Text(text))) => {
                            match serde_json::from_str::<ServerMessage>(&text) {
                                Ok(server_msg) => {
                                    let event_id = server_msg.event_id().map(String::from);
                                    let ws_msg = server_message_to_ws_message(&server_msg, platform);

                                    if let Some(id) = &event_id {
                                        // Events that produce a downstream broadcast pace
                                        // simulation on strategy ACK. Events that do not
                                        // (Error, Pong-with-event_id) must be ACK'd
                                        // immediately — the strategy will never see them
                                        // and registering pending would deadlock the
                                        // engine.
                                        if ws_msg.is_some() {
                                            ack_bridge.register_pending(id.clone()).await;
                                            debug!(event_id = %id, "Event registered pending, waiting for strategy ACK");
                                        } else {
                                            ack_bridge.immediate_ack(id.clone());
                                            debug!(event_id = %id, "Event has no downstream broadcast, immediate ACK");
                                        }
                                    }

                                    // Process the message and send to EventRouter
                                    handle_server_message(&server_msg, &event_router);

                                    // Also send to EventStream for broadcasting to strategies
                                    if let Some(ws_msg) = ws_msg
                                        && event_tx.send(ws_msg).is_err() {
                                            warn!("Event stream closed, stopping message processor");
                                            break;
                                        }
                                },
                                Err(e) => {
                                    warn!(error = %e, text = %text, "Failed to parse ServerMessage");
                                },
                            }
                        },
                        Some(Ok(Message::Ping(data))) => {
                            debug!(
                                len = data.len(),
                                "Received ping from engine"
                            );
                        },
                        Some(Ok(Message::Pong(_))) => {
                            debug!("Received pong from engine");
                        },
                        Some(Ok(Message::Close(frame))) => {
                            info!(frame = ?frame, "Engine WebSocket closed");
                            break;
                        },
                        Some(Ok(Message::Binary(data))) => {
                            warn!(len = data.len(), "Unexpected binary message from engine");
                        },
                        Some(Ok(Message::Frame(_))) => {},
                        Some(Err(e)) => {
                            error!(error = %e, "WebSocket error");
                            break;
                        },
                        None => {
                            warn!("Engine WebSocket stream ended");
                            break;
                        }
                    }
                }
            }
        }

        engine_writer_task.abort();
        info!("Tektii WebSocket provider message processor stopped");
    });
}

// =============================================================================
// Helper Functions for WebSocketProvider Implementation
// =============================================================================

/// Convert a `ServerMessage` to `WsMessage` for subscription filter checking and broadcasting.
fn server_message_to_ws_message(
    msg: &ServerMessage,
    _platform: TradingPlatform,
) -> Option<WsMessage> {
    match msg {
        ServerMessage::Order { event, order, .. } => {
            let api_order = conversions::engine_order_to_api(order);
            let api_event = conversions::engine_order_event_type_to_api(*event);
            Some(WsMessage::Order {
                event: api_event,
                order: api_order,
                parent_order_id: None,
                timestamp: Utc::now(),
            })
        }
        ServerMessage::Trade { trade, .. } => {
            let api_trade = conversions::engine_trade_to_api(trade);
            Some(WsMessage::Trade {
                event: TradeEventType::TradeFilled,
                trade: api_trade,
                timestamp: Utc::now(),
            })
        }
        ServerMessage::Position { position, .. } => {
            let api_position = conversions::engine_position_to_api(position);
            Some(WsMessage::Position {
                event: PositionEventType::PositionModified,
                position: api_position,
                timestamp: Utc::now(),
            })
        }
        ServerMessage::Account { account, .. } => {
            let api_account = conversions::engine_account_to_api(account);
            Some(WsMessage::Account {
                event: AccountEventType::BalanceUpdated,
                account: api_account,
                timestamp: Utc::now(),
            })
        }
        ServerMessage::Candle {
            symbol,
            timeframe,
            timestamp,
            open,
            high,
            low,
            close,
            volume,
            ..
        } => {
            let tf = parse_timeframe(timeframe);
            let bar = Bar {
                symbol: symbol.clone(),
                provider: "tektii".to_string(),
                timeframe: tf,
                timestamp: conversions::unix_ms_to_datetime(*timestamp),
                open: *open,
                high: *high,
                low: *low,
                close: *close,
                volume: Decimal::from_f64(*volume).unwrap_or_default(),
            };
            Some(WsMessage::Candle {
                bar,
                timestamp: Utc::now(),
            })
        }
        ServerMessage::Error { .. } | ServerMessage::Pong => None,
    }
}

/// Handle a `ServerMessage` from the engine by routing through `EventRouter`.
fn handle_server_message(msg: &ServerMessage, event_router: &Arc<EventRouter>) {
    match msg {
        ServerMessage::Order {
            event_id,
            event,
            order,
        } => {
            debug!(
                event_id = %event_id,
                order_id = %order.id,
                event = ?event,
                "Received Order event from engine"
            );

            let api_order = conversions::engine_order_to_api(order);
            let api_event = conversions::engine_order_event_type_to_api(*event);

            let router = event_router.clone();
            tokio::spawn(async move {
                router.handle_order_event(api_event, &api_order, None).await;
            });
        }

        ServerMessage::Trade { event_id, trade } => {
            debug!(
                event_id = %event_id,
                trade_id = %trade.id,
                "Received Trade event from engine"
            );
            // Trade events are broadcast directly via EventStream
        }

        ServerMessage::Position { event_id, position } => {
            debug!(
                event_id = %event_id,
                position_id = %position.id,
                "Received Position event from engine"
            );

            let api_position = conversions::engine_position_to_api(position);

            let router = event_router.clone();
            tokio::spawn(async move {
                router
                    .handle_position_event(PositionEventType::PositionModified, &api_position)
                    .await;
            });
        }

        ServerMessage::Account {
            event_id,
            event,
            account,
        } => {
            debug!(
                event_id = %event_id,
                event = ?event,
                balance = %account.balance,
                "Received Account event from engine"
            );
            // Account events are broadcast directly via EventStream
        }

        ServerMessage::Candle {
            event_id,
            symbol,
            timeframe,
            ..
        } => {
            debug!(
                event_id = %event_id,
                symbol = %symbol,
                timeframe = %timeframe,
                "Received Candle event from engine"
            );
            // Candle events are broadcast directly via EventStream
        }

        ServerMessage::Error {
            event_id,
            code,
            message,
        } => {
            error!(
                event_id = %event_id,
                code = %code,
                message = %message,
                "Received Error event from engine"
            );
        }

        ServerMessage::Pong => {
            debug!("Received Pong from engine");
        }
    }
}
