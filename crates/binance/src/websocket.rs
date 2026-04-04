//! Binance WebSocket provider for real-time market data and user data streaming.
//!
//! Implements the `WebSocketProvider` trait for Binance, supporting:
//! - **Market data** (public, no auth): bookTicker + kline via combined streams
//! - **User data** (private, listen key): executionReport, account updates
//!
//! All 4 platform variants (Spot, Futures, Margin, Coin-Futures) are supported.
//! URL routing is handled by `BinanceUserDataStream`.

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::Serialize;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, RwLock, mpsc};
use tokio::time::sleep;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use secrecy::SecretBox;
use tektii_gateway_core::models::{
    Account, Bar, Order, OrderStatus, OrderType, Quote, Side, TimeInForce, Timeframe,
    TradingPlatform, TradingPlatformKind,
};
use tektii_gateway_core::websocket::error::WebSocketError;
use tektii_gateway_core::websocket::messages::{
    AccountEventType, ConnectionEventType, EventAckMessage, OrderEventType, WsMessage,
};
use tektii_gateway_core::websocket::provider::{EventStream, ProviderConfig, WebSocketProvider};

use crate::map_binance_order_status;
use crate::user_data_stream::BinanceUserDataStream;
use crate::websocket_types::{
    CombinedStreamMessage, FuturesAccountUpdate, FuturesOrderData, FuturesOrderTradeUpdate,
    SpotAccountPositionUpdate, SpotExecutionReport, UserDataEventEnvelope, WsBookTickerEvent,
    WsKlineEvent,
};

// Type alias for the WebSocket write half.
type WsWriteHalf = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;

// ============================================================================
// Connection State
// ============================================================================

#[derive(Debug, Clone, Default)]
enum ConnectionState {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    Subscribed,
    #[allow(dead_code)]
    Error(String),
}

/// Tracks current subscription state.
#[derive(Debug, Clone, Default)]
struct SubscriptionState {
    symbols: std::collections::HashMap<String, Vec<String>>,
}

// ============================================================================
// Subscribe / Unsubscribe Messages
// ============================================================================

#[derive(Debug, Serialize)]
struct BinanceSubscribeMessage {
    method: String,
    params: Vec<String>,
    id: u64,
}

// ============================================================================
// BinanceWebSocketProvider
// ============================================================================

/// Binance WebSocket provider for real-time market data and trading events.
///
/// Manages two separate connections:
/// - **Market data** (public): combined stream for bookTicker + kline
/// - **User data** (private): listen key authenticated stream for order/account events
#[derive(Clone)]
pub struct BinanceWebSocketProvider {
    /// Trading platform (determines URLs).
    platform: TradingPlatform,
    /// User data stream config (URLs, listen key path, API key).
    user_data_stream: Arc<BinanceUserDataStream>,
    /// Market data WebSocket base URL.
    market_ws_base_url: String,
    /// Current subscription state.
    subscriptions: Arc<RwLock<SubscriptionState>>,
    /// Market data connection state.
    connection_state: Arc<RwLock<ConnectionState>>,
    /// Market data write half for sending subscribe/unsubscribe.
    market_write_half: Arc<Mutex<Option<WsWriteHalf>>>,
    /// Cancellation token to stop background tasks on disconnect.
    cancellation_token: CancellationToken,
    /// Stored config from initial `connect()` for reconnection.
    stored_config: Arc<RwLock<Option<ProviderConfig>>>,
    /// Current listen key (if user data stream is connected).
    listen_key: Arc<RwLock<Option<String>>>,
    /// When the listen key was created (for expiry tracking).
    listen_key_created: Arc<RwLock<Option<Instant>>>,
    /// HTTP client for listen key REST calls.
    http_client: reqwest::Client,
    /// Monotonic message ID for subscribe/unsubscribe requests.
    next_msg_id: Arc<AtomicU64>,
}

impl std::fmt::Debug for BinanceWebSocketProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BinanceWebSocketProvider")
            .field("platform", &self.platform)
            .field("market_ws_base_url", &self.market_ws_base_url)
            .field("subscriptions", &self.subscriptions)
            .field("connection_state", &self.connection_state)
            .field("market_write_half", &"<WebSocket>")
            .field("cancellation_token", &"<CancellationToken>")
            .field("stored_config", &"<Config>")
            .field("listen_key", &"<Redacted>")
            .field("listen_key_created", &self.listen_key_created)
            .field("http_client", &"<Client>")
            .field("next_msg_id", &self.next_msg_id)
            .field("user_data_stream", &"<UserDataStream>")
            .finish()
    }
}

impl BinanceWebSocketProvider {
    /// Create a new Binance WebSocket provider for the given platform.
    pub fn new(platform: TradingPlatform, api_key: Arc<SecretBox<String>>) -> Self {
        let user_data_stream = BinanceUserDataStream::new(api_key, platform);
        let market_ws_base_url = user_data_stream.ws_base_url().to_string();

        Self {
            platform,
            user_data_stream: Arc::new(user_data_stream),
            market_ws_base_url,
            subscriptions: Arc::new(RwLock::new(SubscriptionState::default())),
            connection_state: Arc::new(RwLock::new(ConnectionState::default())),
            market_write_half: Arc::new(Mutex::new(None)),
            cancellation_token: CancellationToken::new(),
            stored_config: Arc::new(RwLock::new(None)),
            listen_key: Arc::new(RwLock::new(None)),
            listen_key_created: Arc::new(RwLock::new(None)),
            http_client: reqwest::Client::new(),
            next_msg_id: Arc::new(AtomicU64::new(1)),
        }
    }

    // ========================================================================
    // Market Data Connection
    // ========================================================================

    /// Connect to the combined market data stream.
    ///
    /// Returns `(tx, rx)` where events are sent via `tx` and consumed via `rx`.
    async fn connect_market_data(
        &self,
        config: &ProviderConfig,
    ) -> Result<(mpsc::UnboundedSender<WsMessage>, EventStream), WebSocketError> {
        *self.connection_state.write().await = ConnectionState::Connecting;

        let stream_names = self.build_stream_names(&config.symbols, &config.event_types);
        let url = if stream_names.is_empty() {
            // No market data subscriptions — just connect to base for user data passthrough.
            format!("{}/ws", self.market_ws_base_url)
        } else {
            let streams_param = stream_names.join("/");
            format!("{}/stream?streams={streams_param}", self.market_ws_base_url)
        };

        info!(platform = ?self.platform, %url, "Connecting to Binance market data WebSocket");

        let ws_stream = self.connect_with_retry(&url).await?;

        *self.connection_state.write().await = ConnectionState::Connected;
        let (write, mut read) = ws_stream.split();
        *self.market_write_half.lock().await = Some(write);

        // Update subscription state
        {
            let mut subs = self.subscriptions.write().await;
            for symbol in &config.symbols {
                subs.symbols
                    .insert(symbol.clone(), config.event_types.clone());
            }
        }
        *self.connection_state.write().await = ConnectionState::Subscribed;

        let (tx, rx) = mpsc::unbounded_channel();
        let cancel_token = self.cancellation_token.clone();
        let connection_state = self.connection_state.clone();
        let platform = self.platform;

        // Spawn read task for market data
        let tx_clone = tx.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = cancel_token.cancelled() => {
                        *connection_state.write().await = ConnectionState::Disconnected;
                        break;
                    }
                    msg_result = read.next() => {
                        match msg_result {
                            Some(Ok(Message::Text(text))) => {
                                for event in process_market_message(&text, platform) {
                                    if tx_clone.send(event).is_err() {
                                        return;
                                    }
                                }
                            }
                            Some(Ok(Message::Close(frame))) => {
                                info!(platform = ?platform, "Market data WebSocket closed: {frame:?}");
                                *connection_state.write().await = ConnectionState::Disconnected;
                                // Emit disconnect event
                                let _ = tx_clone.send(WsMessage::Connection {
                                    event: ConnectionEventType::BrokerDisconnected,
                                    error: Some("Market data stream closed".to_string()),
                                    broker: Some(format!("{platform:?}")),
                                    gap_duration_ms: None,
                                    timestamp: Utc::now(),
                                });
                                return;
                            }
                            Some(Err(e)) => {
                                error!(platform = ?platform, "Market data WebSocket error: {e}");
                                *connection_state.write().await = ConnectionState::Error(format!("{e}"));
                                let _ = tx_clone.send(WsMessage::Connection {
                                    event: ConnectionEventType::BrokerDisconnected,
                                    error: Some(format!("WebSocket error: {e}")),
                                    broker: Some(format!("{platform:?}")),
                                    gap_duration_ms: None,
                                    timestamp: Utc::now(),
                                });
                                return;
                            }
                            None => {
                                *connection_state.write().await = ConnectionState::Disconnected;
                                return;
                            }
                            _ => {}
                        }
                    }
                }
            }
        });

        Ok((tx, rx))
    }

    // ========================================================================
    // User Data Stream Connection
    // ========================================================================

    /// Connect to the user data stream (listen key authenticated).
    ///
    /// Spawns background tasks for:
    /// - Reading user data events (order/account updates)
    /// - Listen key keepalive (every 20 minutes with jitter)
    async fn connect_user_data(
        &self,
        tx: mpsc::UnboundedSender<WsMessage>,
    ) -> Result<(), WebSocketError> {
        let listen_key = self
            .user_data_stream
            .create_listen_key(&self.http_client)
            .await?;

        let ws_url = self.user_data_stream.ws_url(&listen_key);
        info!(platform = ?self.platform, "Connecting to Binance user data stream");

        let ws_stream = self.connect_with_retry(&ws_url).await?;
        let (_write, mut read) = ws_stream.split();

        *self.listen_key.write().await = Some(listen_key.clone());
        *self.listen_key_created.write().await = Some(Instant::now());

        let cancel_token = self.cancellation_token.clone();
        let platform = self.platform;
        let is_futures = matches!(
            platform.kind(),
            TradingPlatformKind::BinanceFutures | TradingPlatformKind::BinanceCoinFutures
        );

        // Spawn read task for user data
        let tx_read = tx.clone();
        let cancel_token_read = cancel_token.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = cancel_token_read.cancelled() => { break; }
                    msg_result = read.next() => {
                        match msg_result {
                            Some(Ok(Message::Text(text))) => {
                                for event in process_user_data_message(&text, platform, is_futures) {
                                    if tx_read.send(event).is_err() {
                                        return;
                                    }
                                }
                            }
                            Some(Ok(Message::Close(frame))) => {
                                warn!(platform = ?platform, "User data stream closed: {frame:?}");
                                return;
                            }
                            Some(Err(e)) => {
                                error!(platform = ?platform, "User data stream error: {e}");
                                return;
                            }
                            None => { return; }
                            _ => {}
                        }
                    }
                }
            }
        });

        // Spawn listen key keepalive task (every 20 minutes + jitter)
        let uds = self.user_data_stream.clone();
        let http_client = self.http_client.clone();
        let listen_key_ref = self.listen_key.clone();
        let listen_key_created_ref = self.listen_key_created.clone();
        tokio::spawn(async move {
            // 20 minutes base interval
            let base_interval = Duration::from_secs(20 * 60);
            loop {
                // Add jitter: 0-60 seconds
                let jitter = Duration::from_secs(rand_jitter_secs());
                let interval = base_interval + jitter;

                tokio::select! {
                    () = cancel_token.cancelled() => { break; }
                    () = sleep(interval) => {
                        let key = listen_key_ref.read().await.clone();
                        if let Some(key) = key
                            && let Err(e) = uds.keepalive_listen_key(&http_client, &key).await
                        {
                            warn!(platform = ?platform, "Listen key keepalive failed: {e}, creating new key");
                            match uds.create_listen_key(&http_client).await {
                                Ok(new_key) => {
                                    *listen_key_ref.write().await = Some(new_key);
                                    *listen_key_created_ref.write().await = Some(Instant::now());
                                    info!(platform = ?platform, "Created new listen key after keepalive failure");
                                }
                                Err(e) => {
                                    error!(platform = ?platform, "Failed to create new listen key: {e}");
                                }
                            }
                        }
                    }
                }
            }
        });

        Ok(())
    }

    // ========================================================================
    // Helpers
    // ========================================================================

    /// Connect to a WebSocket URL with retry + exponential backoff.
    async fn connect_with_retry(
        &self,
        url: &str,
    ) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, WebSocketError> {
        const MAX_RETRIES: u32 = 5;
        const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
        let mut retry_count = 0;

        loop {
            match connect_async(url).await {
                Ok((stream, _)) => return Ok(stream),
                Err(e) => {
                    retry_count += 1;
                    if retry_count >= MAX_RETRIES {
                        return Err(WebSocketError::ConnectionFailed(format!(
                            "Failed to connect after {MAX_RETRIES} retries: {e}"
                        )));
                    }
                    let backoff = INITIAL_BACKOFF * 2_u32.pow(retry_count - 1);
                    warn!(
                        platform = ?self.platform,
                        "Connection failed (attempt {retry_count}/{MAX_RETRIES}), retrying in {backoff:?}: {e}"
                    );
                    sleep(backoff).await;
                }
            }
        }
    }

    /// Build Binance stream names from gateway event types and symbols.
    ///
    /// Maps: `candle` -> `{sym}@kline_1m`, `quote` -> `{sym}@bookTicker`
    fn build_stream_names(&self, symbols: &[String], event_types: &[String]) -> Vec<String> {
        let mut streams = Vec::new();

        for symbol in symbols {
            let sym_lower = symbol.to_lowercase();
            for event_type in event_types {
                match event_type.as_str() {
                    "quote" | "quotes" | "bookTicker" => {
                        streams.push(format!("{sym_lower}@bookTicker"));
                    }
                    "candle" | "candles" | "bar" | "bars" | "candle_1m" => {
                        streams.push(format!("{sym_lower}@kline_1m"));
                    }
                    "candle_5m" => streams.push(format!("{sym_lower}@kline_5m")),
                    "candle_15m" => streams.push(format!("{sym_lower}@kline_15m")),
                    "candle_1h" => streams.push(format!("{sym_lower}@kline_1h")),
                    "candle_1d" => streams.push(format!("{sym_lower}@kline_1d")),
                    // order_update, trade_update, account_update are handled by user data stream
                    "order_update" | "trade_update" | "account_update" => {}
                    other => {
                        warn!(platform = ?self.platform, "Unknown event type: {other}");
                    }
                }
            }
        }

        streams
    }
}

// ============================================================================
// WebSocketProvider Trait Implementation
// ============================================================================

#[async_trait]
impl WebSocketProvider for BinanceWebSocketProvider {
    async fn connect(&self, config: ProviderConfig) -> Result<EventStream, WebSocketError> {
        // Store config for reconnection
        *self.stored_config.write().await = Some(config.clone());

        // Connect market data stream
        let (tx, rx) = self.connect_market_data(&config).await?;

        // Connect user data stream if credentials provided
        if config.credentials.is_some()
            && let Err(e) = self.connect_user_data(tx).await
        {
            warn!(platform = ?self.platform, "Failed to connect user data stream: {e}");
            // Market data still works — log but don't fail
        }

        // Emit connected event
        info!(platform = ?self.platform, "Binance WebSocket provider connected");

        Ok(rx)
    }

    async fn subscribe(
        &self,
        symbols: Vec<String>,
        event_types: Vec<String>,
    ) -> Result<(), WebSocketError> {
        let stream_names = self.build_stream_names(&symbols, &event_types);
        if stream_names.is_empty() {
            return Ok(());
        }

        let msg = BinanceSubscribeMessage {
            method: "SUBSCRIBE".to_string(),
            params: stream_names,
            id: self.next_msg_id.fetch_add(1, Ordering::Relaxed),
        };

        let json = serde_json::to_string(&msg).map_err(WebSocketError::MessageError)?;

        let mut write_guard = self.market_write_half.lock().await;
        let write = write_guard
            .as_mut()
            .ok_or_else(|| WebSocketError::ConnectionClosed("Not connected".to_string()))?;

        write
            .send(Message::Text(json.into()))
            .await
            .map_err(|e| WebSocketError::SendError(format!("Failed to send subscribe: {e}")))?;
        drop(write_guard);

        // Update subscription state
        let mut subs = self.subscriptions.write().await;
        for symbol in symbols {
            subs.symbols.insert(symbol, event_types.clone());
        }
        drop(subs);

        Ok(())
    }

    async fn unsubscribe(&self, symbols: Vec<String>) -> Result<(), WebSocketError> {
        let subs = self.subscriptions.read().await;
        let event_types: Vec<String> = symbols
            .iter()
            .filter_map(|sym| subs.symbols.get(sym))
            .flat_map(std::clone::Clone::clone)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        drop(subs);

        let stream_names = self.build_stream_names(&symbols, &event_types);
        if stream_names.is_empty() {
            return Ok(());
        }

        let msg = BinanceSubscribeMessage {
            method: "UNSUBSCRIBE".to_string(),
            params: stream_names,
            id: self.next_msg_id.fetch_add(1, Ordering::Relaxed),
        };

        let json = serde_json::to_string(&msg).map_err(WebSocketError::MessageError)?;

        let mut write_guard = self.market_write_half.lock().await;
        let write = write_guard
            .as_mut()
            .ok_or_else(|| WebSocketError::ConnectionClosed("Not connected".to_string()))?;

        write
            .send(Message::Text(json.into()))
            .await
            .map_err(|e| WebSocketError::SendError(format!("Failed to send unsubscribe: {e}")))?;
        drop(write_guard);

        let mut subs_write = self.subscriptions.write().await;
        for symbol in symbols {
            subs_write.symbols.remove(&symbol);
        }
        drop(subs_write);

        Ok(())
    }

    async fn handle_ack(&self, ack: EventAckMessage) -> Result<(), WebSocketError> {
        debug!(
            platform = ?self.platform,
            "Received ACK for {} events (informational only for live trading)",
            ack.events_processed.len()
        );
        Ok(())
    }

    async fn disconnect(&self) -> Result<(), WebSocketError> {
        self.cancellation_token.cancel();
        *self.connection_state.write().await = ConnectionState::Disconnected;
        *self.market_write_half.lock().await = None;

        // Delete listen key if we have one
        let current_key = self.listen_key.read().await.clone();
        if let Some(key) = current_key {
            if let Err(e) = self
                .user_data_stream
                .delete_listen_key(&self.http_client, &key)
                .await
            {
                warn!(platform = ?self.platform, "Failed to delete listen key on disconnect: {e}");
            }
            *self.listen_key.write().await = None;
        }

        info!(platform = ?self.platform, "Binance WebSocket provider disconnected");
        Ok(())
    }

    async fn reconnect(&self) -> Result<EventStream, WebSocketError> {
        let config = self.stored_config.read().await.clone().ok_or_else(|| {
            WebSocketError::ConnectionClosed(
                "Cannot reconnect: no stored config (connect() was never called)".to_string(),
            )
        })?;

        // Clear stale state
        *self.connection_state.write().await = ConnectionState::Disconnected;
        *self.market_write_half.lock().await = None;

        // Re-establish market data
        let (tx, rx) = self.connect_market_data(&config).await?;

        // Re-establish user data stream if credentials present
        if config.credentials.is_some() {
            // Reuse listen key if it's less than 50 minutes old
            let should_reuse = {
                let created = self.listen_key_created.read().await;
                let key = self.listen_key.read().await;
                key.is_some()
                    && created
                        .map(|c| c.elapsed() < Duration::from_secs(50 * 60))
                        .unwrap_or(false)
            };

            if !should_reuse {
                // Need a new listen key
                *self.listen_key.write().await = None;
            }

            if let Err(e) = self.connect_user_data(tx).await {
                warn!(platform = ?self.platform, "Failed to reconnect user data stream: {e}");
            }
        }

        info!(platform = ?self.platform, "Binance WebSocket provider reconnected");
        Ok(rx)
    }
}

// ============================================================================
// Message Processing (free functions for testability)
// ============================================================================

/// Process a market data message from the combined stream.
pub fn process_market_message(text: &str, platform: TradingPlatform) -> Vec<WsMessage> {
    let Ok(combined) = serde_json::from_str::<CombinedStreamMessage>(text) else {
        // Might be a subscription response like {"result":null,"id":1}
        debug!(platform = ?platform, "Non-stream message: {}", &text[..text.len().min(200)]);
        return vec![];
    };

    let stream = &combined.stream;
    let provider_name = platform_provider_name(platform);

    if stream.ends_with("@bookTicker") {
        match serde_json::from_value::<WsBookTickerEvent>(combined.data) {
            Ok(ticker) => {
                let bid = Decimal::from_str(&ticker.bid_price).unwrap_or_default();
                let ask = Decimal::from_str(&ticker.ask_price).unwrap_or_default();
                let mid = (bid + ask) / Decimal::from(2);

                vec![WsMessage::QuoteData {
                    quote: Quote {
                        symbol: ticker.symbol,
                        provider: provider_name.to_string(),
                        bid,
                        bid_size: Decimal::from_str(&ticker.bid_qty).ok(),
                        ask,
                        ask_size: Decimal::from_str(&ticker.ask_qty).ok(),
                        last: mid,
                        volume: None,
                        timestamp: Utc::now(),
                    },
                    timestamp: Utc::now(),
                }]
            }
            Err(e) => {
                warn!(platform = ?platform, "Failed to parse bookTicker: {e}");
                vec![]
            }
        }
    } else if stream.contains("@kline_") {
        match serde_json::from_value::<WsKlineEvent>(combined.data) {
            Ok(kline) => {
                let timeframe = binance_interval_to_timeframe(&kline.kline.interval);
                let timestamp = millis_to_datetime(kline.kline.open_time);

                vec![WsMessage::Candle {
                    bar: Bar {
                        symbol: kline.symbol,
                        provider: provider_name.to_string(),
                        timeframe,
                        timestamp,
                        open: Decimal::from_str(&kline.kline.open).unwrap_or_default(),
                        high: Decimal::from_str(&kline.kline.high).unwrap_or_default(),
                        low: Decimal::from_str(&kline.kline.low).unwrap_or_default(),
                        close: Decimal::from_str(&kline.kline.close).unwrap_or_default(),
                        volume: Decimal::from_str(&kline.kline.volume).unwrap_or_default(),
                    },
                    timestamp: Utc::now(),
                }]
            }
            Err(e) => {
                warn!(platform = ?platform, "Failed to parse kline: {e}");
                vec![]
            }
        }
    } else {
        debug!(platform = ?platform, "Unknown stream: {stream}");
        vec![]
    }
}

/// Process a user data stream message.
pub fn process_user_data_message(
    text: &str,
    platform: TradingPlatform,
    is_futures: bool,
) -> Vec<WsMessage> {
    // Peek at event type first
    let envelope: UserDataEventEnvelope = match serde_json::from_str(text) {
        Ok(e) => e,
        Err(e) => {
            warn!(platform = ?platform, "Failed to parse user data event envelope: {e}");
            return vec![];
        }
    };

    match envelope.event_type.as_str() {
        // Spot / Margin
        "executionReport" if !is_futures => parse_spot_execution_report(text, platform),
        "outboundAccountPosition" if !is_futures => parse_spot_account_update(text, platform),

        // Futures / Coin-Futures
        "ORDER_TRADE_UPDATE" if is_futures => parse_futures_order_update(text, platform),
        "ACCOUNT_UPDATE" if is_futures => parse_futures_account_update(text, platform),

        // Events we don't handle yet
        "balanceUpdate" | "ACCOUNT_CONFIG_UPDATE" | "STRATEGY_UPDATE" | "listenKeyExpired" => {
            if envelope.event_type == "listenKeyExpired" {
                warn!(platform = ?platform, "Listen key expired — user data stream will disconnect");
            } else {
                debug!(platform = ?platform, "Ignoring user data event: {}", envelope.event_type);
            }
            vec![]
        }

        other => {
            debug!(platform = ?platform, "Unknown user data event type: {other}");
            vec![]
        }
    }
}

// ============================================================================
// Spot / Margin Parsers
// ============================================================================

fn parse_spot_execution_report(text: &str, platform: TradingPlatform) -> Vec<WsMessage> {
    let report: SpotExecutionReport = match serde_json::from_str(text) {
        Ok(r) => r,
        Err(e) => {
            warn!(platform = ?platform, "Failed to parse executionReport: {e}");
            return vec![];
        }
    };

    let status = match map_binance_order_status(&report.order_status) {
        Ok(s) => s,
        Err(e) => {
            warn!(platform = ?platform, "Unknown order status: {e}");
            return vec![];
        }
    };

    let event_type = binance_execution_to_event_type(&report.execution_type, &report.order_status);
    let order = build_order_from_spot_report(&report, status);

    vec![WsMessage::Order {
        event: event_type,
        order,
        parent_order_id: None,
        timestamp: millis_to_datetime(report.transaction_time),
    }]
}

fn parse_spot_account_update(text: &str, platform: TradingPlatform) -> Vec<WsMessage> {
    let update: SpotAccountPositionUpdate = match serde_json::from_str(text) {
        Ok(u) => u,
        Err(e) => {
            warn!(platform = ?platform, "Failed to parse outboundAccountPosition: {e}");
            return vec![];
        }
    };

    // Sum up all free + locked balances (simplified — strategy can query full state via REST)
    let total_balance: Decimal = update
        .balances
        .iter()
        .filter_map(|b| Decimal::from_str(&b.free).ok())
        .sum();

    vec![WsMessage::Account {
        event: AccountEventType::BalanceUpdated,
        account: Account {
            balance: total_balance,
            equity: total_balance,
            margin_used: Decimal::ZERO,
            margin_available: total_balance,
            unrealized_pnl: Decimal::ZERO,
            currency: "USDT".to_string(),
        },
        timestamp: Utc::now(),
    }]
}

// ============================================================================
// Futures / Coin-Futures Parsers
// ============================================================================

fn parse_futures_order_update(text: &str, platform: TradingPlatform) -> Vec<WsMessage> {
    let update: FuturesOrderTradeUpdate = match serde_json::from_str(text) {
        Ok(u) => u,
        Err(e) => {
            warn!(platform = ?platform, "Failed to parse ORDER_TRADE_UPDATE: {e}");
            return vec![];
        }
    };

    let o = &update.order;
    let status = match map_binance_order_status(&o.order_status) {
        Ok(s) => s,
        Err(e) => {
            warn!(platform = ?platform, "Unknown order status: {e}");
            return vec![];
        }
    };

    let event_type = binance_execution_to_event_type(&o.execution_type, &o.order_status);
    let order = build_order_from_futures_data(o, status);

    vec![WsMessage::Order {
        event: event_type,
        order,
        parent_order_id: None,
        timestamp: Utc::now(),
    }]
}

fn parse_futures_account_update(text: &str, platform: TradingPlatform) -> Vec<WsMessage> {
    let update: FuturesAccountUpdate = match serde_json::from_str(text) {
        Ok(u) => u,
        Err(e) => {
            warn!(platform = ?platform, "Failed to parse ACCOUNT_UPDATE: {e}");
            return vec![];
        }
    };

    let mut events = Vec::new();

    // Balance update
    if let Some(balance) = update.account.balances.first() {
        let wallet = Decimal::from_str(&balance.wallet_balance).unwrap_or_default();
        events.push(WsMessage::Account {
            event: AccountEventType::BalanceUpdated,
            account: Account {
                balance: wallet,
                equity: wallet,
                margin_used: Decimal::ZERO,
                margin_available: wallet,
                unrealized_pnl: Decimal::ZERO,
                currency: balance.asset.clone(),
            },
            timestamp: Utc::now(),
        });
    }

    events
}

// ============================================================================
// Order Building Helpers
// ============================================================================

fn build_order_from_spot_report(report: &SpotExecutionReport, status: OrderStatus) -> Order {
    let side = match report.side.as_str() {
        "BUY" => Side::Buy,
        _ => Side::Sell,
    };

    let order_type = binance_order_type(&report.order_type);
    let quantity = Decimal::from_str(&report.quantity).unwrap_or_default();
    let filled_quantity = Decimal::from_str(&report.cumulative_filled_qty).unwrap_or_default();
    let remaining = (quantity - filled_quantity).max(Decimal::ZERO);

    // For cancels, use the original client order ID
    let client_order_id =
        if report.execution_type == "CANCELED" && !report.original_client_order_id.is_empty() {
            Some(report.original_client_order_id.clone())
        } else if !report.client_order_id.is_empty() {
            Some(report.client_order_id.clone())
        } else {
            None
        };

    // Calculate average fill price from last executed price for fills
    let average_fill_price = if filled_quantity > Decimal::ZERO {
        Decimal::from_str(&report.last_executed_price).ok()
    } else {
        None
    };

    let limit_price = Decimal::from_str(&report.price)
        .ok()
        .filter(|p| *p > Decimal::ZERO);
    let stop_price = Decimal::from_str(&report.stop_price)
        .ok()
        .filter(|p| *p > Decimal::ZERO);

    let now = millis_to_datetime(report.transaction_time);

    Order {
        id: report.order_id.to_string(),
        client_order_id,
        symbol: report.symbol.clone(),
        side,
        order_type,
        quantity,
        filled_quantity,
        remaining_quantity: remaining,
        limit_price,
        stop_price,
        stop_loss: None,
        take_profit: None,
        trailing_distance: None,
        trailing_type: None,
        average_fill_price,
        status,
        reject_reason: None,
        position_id: None,
        reduce_only: None,
        post_only: None,
        hidden: None,
        display_quantity: None,
        oco_group_id: None,
        correlation_id: None,
        time_in_force: binance_time_in_force(&report.time_in_force),
        created_at: now,
        updated_at: now,
    }
}

fn build_order_from_futures_data(o: &FuturesOrderData, status: OrderStatus) -> Order {
    let side = match o.side.as_str() {
        "BUY" => Side::Buy,
        _ => Side::Sell,
    };

    let order_type = binance_order_type(&o.order_type);
    let quantity = Decimal::from_str(&o.quantity).unwrap_or_default();
    let filled_quantity = Decimal::from_str(&o.cumulative_filled_qty).unwrap_or_default();
    let remaining = (quantity - filled_quantity).max(Decimal::ZERO);
    let average_fill_price = Decimal::from_str(&o.average_price)
        .ok()
        .filter(|p| *p > Decimal::ZERO);
    let limit_price = Decimal::from_str(&o.price)
        .ok()
        .filter(|p| *p > Decimal::ZERO);
    let stop_price = Decimal::from_str(&o.stop_price)
        .ok()
        .filter(|p| *p > Decimal::ZERO);

    // Trailing stop fields
    let trailing_distance = o
        .callback_rate
        .as_ref()
        .and_then(|r| Decimal::from_str(r).ok());
    let trailing_type = if trailing_distance.is_some() {
        Some(tektii_gateway_core::models::TrailingType::Percent)
    } else {
        None
    };

    let client_order_id = if o.client_order_id.is_empty() {
        None
    } else {
        Some(o.client_order_id.clone())
    };

    let now = Utc::now();

    Order {
        id: o.order_id.to_string(),
        client_order_id,
        symbol: o.symbol.clone(),
        side,
        order_type,
        quantity,
        filled_quantity,
        remaining_quantity: remaining,
        limit_price,
        stop_price,
        stop_loss: None,
        take_profit: None,
        trailing_distance,
        trailing_type,
        average_fill_price,
        status,
        reject_reason: None,
        position_id: None,
        reduce_only: Some(o.reduce_only),
        post_only: None,
        hidden: None,
        display_quantity: None,
        oco_group_id: None,
        correlation_id: None,
        time_in_force: binance_time_in_force(&o.time_in_force),
        created_at: now,
        updated_at: now,
    }
}

// ============================================================================
// Mapping Helpers
// ============================================================================

/// Map Binance execution type + order status to gateway `OrderEventType`.
fn binance_execution_to_event_type(execution_type: &str, order_status: &str) -> OrderEventType {
    match execution_type {
        "NEW" => OrderEventType::OrderCreated,
        "TRADE" => {
            if order_status == "FILLED" {
                OrderEventType::OrderFilled
            } else {
                OrderEventType::OrderPartiallyFilled
            }
        }
        "CANCELED" => OrderEventType::OrderCancelled,
        "REJECTED" => OrderEventType::OrderRejected,
        "EXPIRED" => OrderEventType::OrderExpired,
        _ => OrderEventType::OrderModified,
    }
}

/// Map Binance order type string to gateway `OrderType`.
fn binance_order_type(order_type: &str) -> OrderType {
    match order_type {
        "LIMIT" => OrderType::Limit,
        "STOP_LOSS" | "STOP" | "STOP_MARKET" => OrderType::Stop,
        "STOP_LOSS_LIMIT" | "STOP_LIMIT" => OrderType::StopLimit,
        "TRAILING_STOP_MARKET" => OrderType::TrailingStop,
        // MARKET and anything unrecognised
        _ => OrderType::Market,
    }
}

/// Map Binance time-in-force to gateway `TimeInForce`.
fn binance_time_in_force(tif: &str) -> TimeInForce {
    match tif {
        "IOC" => TimeInForce::Ioc,
        "FOK" => TimeInForce::Fok,
        // GTC, GTE_GTC, and anything unrecognised
        _ => TimeInForce::Gtc,
    }
}

/// Map Binance kline interval to gateway `Timeframe`.
fn binance_interval_to_timeframe(interval: &str) -> Timeframe {
    interval
        .parse::<Timeframe>()
        .unwrap_or(Timeframe::OneMinute)
}

/// Convert milliseconds timestamp to `DateTime<Utc>`.
fn millis_to_datetime(millis: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(millis)
        .single()
        .unwrap_or_else(Utc::now)
}

/// Provider name string for a platform.
const fn platform_provider_name(platform: TradingPlatform) -> &'static str {
    match platform.kind() {
        TradingPlatformKind::BinanceSpot => "binance-spot",
        TradingPlatformKind::BinanceFutures => "binance-futures",
        TradingPlatformKind::BinanceMargin => "binance-margin",
        TradingPlatformKind::BinanceCoinFutures => "binance-coin-futures",
        _ => "binance",
    }
}

/// Simple jitter: 0-60 seconds based on system time nanos.
fn rand_jitter_secs() -> u64 {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or(Duration::from_secs(0))
        .subsec_nanos();
    u64::from(nanos % 60)
}
