//! Alpaca WebSocket provider for real-time market data and trading events.
//!
//! Implements the `WebSocketProvider` trait for Alpaca, supporting:
//! - **Market data stream** (authenticated): quotes, bars, trades via SIP/IEX feeds
//! - **Trading stream** (authenticated): order updates via Alpaca trading events API
//!
//! Both paper and live platforms are supported. The feed (SIP vs IEX) is
//! configurable via the `feed` field on `AlpacaCredentials`.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, RwLock, mpsc};
use tokio::time::sleep;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use tektii_gateway_core::models::{
    Bar, Order, OrderStatus, OrderType, Quote, Side, TimeInForce, TradingPlatform,
};
use tektii_gateway_core::websocket::error::WebSocketError;
use tektii_gateway_core::websocket::messages::{
    ConnectionEventType, EventAckMessage, InternalTradingEvent, OrderContext, OrderEventType,
    WsMessage,
};
use tektii_gateway_core::websocket::provider::{
    Credentials, EventStream, ProviderConfig, ProviderEvent, WebSocketProvider,
};

/// Type alias for the WebSocket write half.
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
    Authenticated,
    Subscribed,
    #[allow(dead_code)]
    Error(String),
}

/// Tracks current subscription state.
#[derive(Debug, Clone, Default)]
struct SubscriptionState {
    symbols: Vec<String>,
    event_types: Vec<String>,
}

// ============================================================================
// Alpaca Market Data Protocol Messages
// ============================================================================

/// Authentication message sent to Alpaca market data WebSocket.
#[derive(Debug, Serialize)]
struct AlpacaAuthMessage {
    action: String,
    key: String,
    secret: String,
}

/// Subscription message sent to Alpaca market data WebSocket.
#[derive(Debug, Serialize)]
struct AlpacaSubscribeMessage {
    action: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    trades: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    quotes: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    bars: Vec<String>,
}

/// Trade message from Alpaca market data stream.
#[derive(Debug, Deserialize)]
struct AlpacaTradeMessage {
    /// Message type discriminator (always "t" for trades).
    #[serde(rename = "T")]
    #[allow(dead_code)]
    msg_type: String,
    #[serde(rename = "S")]
    symbol: String,
    #[serde(rename = "p")]
    price: f64,
    #[serde(rename = "s")]
    size: f64,
    #[serde(rename = "t")]
    timestamp: String,
}

/// Quote message from Alpaca market data stream.
#[derive(Debug, Deserialize)]
struct AlpacaQuoteMessage {
    /// Message type discriminator (always "q" for quotes).
    #[serde(rename = "T")]
    #[allow(dead_code)]
    msg_type: String,
    #[serde(rename = "S")]
    symbol: String,
    #[serde(rename = "bp")]
    bid_price: f64,
    #[serde(rename = "bs")]
    bid_size: f64,
    #[serde(rename = "ap")]
    ask_price: f64,
    #[serde(rename = "as")]
    ask_size: f64,
    #[serde(rename = "t")]
    timestamp: String,
}

/// Bar (candlestick) message from Alpaca market data stream.
#[derive(Debug, Deserialize)]
struct AlpacaBarMessage {
    /// Message type discriminator (always "b" for bars).
    #[serde(rename = "T")]
    #[allow(dead_code)]
    msg_type: String,
    #[serde(rename = "S")]
    symbol: String,
    #[serde(rename = "o")]
    open: f64,
    #[serde(rename = "h")]
    high: f64,
    #[serde(rename = "l")]
    low: f64,
    #[serde(rename = "c")]
    close: f64,
    #[serde(rename = "v")]
    volume: f64,
    #[serde(rename = "t")]
    timestamp: String,
}

// ============================================================================
// Alpaca Trading Stream Protocol Messages
// ============================================================================

/// Authentication message for Alpaca trading stream.
#[derive(Debug, Serialize)]
struct TradingAuthMessage {
    action: String,
    data: TradingAuthData,
}

/// Auth data payload for trading stream.
#[derive(Debug, Serialize)]
struct TradingAuthData {
    key_id: String,
    secret_key: String,
}

/// Listen message for Alpaca trading stream (subscribe to trade updates).
#[derive(Debug, Serialize)]
struct TradingListenMessage {
    action: String,
    data: TradingListenData,
}

/// Listen data payload for trading stream.
#[derive(Debug, Serialize)]
struct TradingListenData {
    streams: Vec<String>,
}

/// Top-level message from Alpaca trading stream.
#[derive(Debug, Deserialize)]
struct AlpacaTradingStreamMessage {
    stream: String,
    #[serde(default)]
    data: Option<serde_json::Value>,
}

/// Order update data from Alpaca trading stream.
#[derive(Debug, Deserialize)]
struct AlpacaTradingOrderUpdate {
    event: String,
    order: AlpacaTradingOrderInfo,
    /// Current position quantity after this fill event (used for position synthesis).
    #[serde(default)]
    #[allow(dead_code)]
    position_qty: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
    /// Fill quantity for this specific execution event.
    #[serde(default)]
    #[allow(dead_code)]
    qty: Option<String>,
    /// Fill price for this specific execution event.
    #[serde(default)]
    #[allow(dead_code)]
    price: Option<String>,
}

/// Order info nested within trading stream update.
#[derive(Debug, Deserialize)]
struct AlpacaTradingOrderInfo {
    id: String,
    #[serde(default)]
    client_order_id: Option<String>,
    symbol: String,
    side: String,
    #[serde(rename = "type")]
    order_type: String,
    #[serde(default)]
    qty: Option<String>,
    #[serde(default)]
    filled_qty: Option<String>,
    #[serde(default)]
    filled_avg_price: Option<String>,
    #[serde(default)]
    limit_price: Option<String>,
    #[serde(default)]
    stop_price: Option<String>,
    status: String,
    time_in_force: String,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
    /// Leg orders for bracket/OTO orders.
    #[serde(default)]
    #[allow(dead_code)]
    legs: Option<Vec<AlpacaTradingOrderInfo>>,
}

// ============================================================================
// AlpacaWebSocketProvider
// ============================================================================

/// Alpaca WebSocket provider for real-time market data and trading events.
///
/// Manages two separate authenticated WebSocket connections:
/// - **Market data** (`stream.data.alpaca.markets`): quotes, bars, trades
/// - **Trading stream** (`api.alpaca.markets/stream`): order updates
#[derive(Clone)]
pub struct AlpacaWebSocketProvider {
    /// Feed type: "iex" (free) or "sip" (paid).
    feed: String,
    /// Base URL override for testing.
    base_url: Option<String>,
    /// Current subscription state.
    subscriptions: Arc<RwLock<SubscriptionState>>,
    /// Market data connection state.
    connection_state: Arc<RwLock<ConnectionState>>,
    /// Market data write half for sending subscribe/unsubscribe.
    write_half: Arc<Mutex<Option<WsWriteHalf>>>,
    /// Last pong timestamp for health checking.
    last_pong: Arc<RwLock<Option<SystemTime>>>,
    /// Ping interval in seconds.
    ping_interval: u64,
    /// Ping timeout in seconds.
    ping_timeout: u64,
    /// Cancellation token to stop background tasks on disconnect.
    cancellation_token: CancellationToken,
    /// Trading stream connection state.
    trading_connection_state: Arc<RwLock<ConnectionState>>,
    /// Whether trading stream is enabled (has credentials).
    trading_stream_enabled: Arc<RwLock<bool>>,
    /// Stored config from initial `connect()` for reconnection.
    stored_config: Arc<RwLock<Option<ProviderConfig>>>,
}

impl std::fmt::Debug for AlpacaWebSocketProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlpacaWebSocketProvider")
            .field("feed", &self.feed)
            .field("base_url", &self.base_url)
            .field("subscriptions", &self.subscriptions)
            .field("connection_state", &self.connection_state)
            .field("write_half", &"<WebSocket>")
            .field("last_pong", &self.last_pong)
            .field("ping_interval", &self.ping_interval)
            .field("ping_timeout", &self.ping_timeout)
            .field("cancellation_token", &"<CancellationToken>")
            .field("trading_connection_state", &self.trading_connection_state)
            .field("trading_stream_enabled", &self.trading_stream_enabled)
            .field("stored_config", &"<Config>")
            .finish()
    }
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Convert an Alpaca order info to a gateway `Order`.
fn alpaca_order_to_trading_order(info: &AlpacaTradingOrderInfo) -> Order {
    let side = match info.side.to_lowercase().as_str() {
        "buy" => Side::Buy,
        _ => Side::Sell,
    };

    let order_type = match info.order_type.to_lowercase().as_str() {
        "limit" => OrderType::Limit,
        "stop" => OrderType::Stop,
        "stop_limit" => OrderType::StopLimit,
        "trailing_stop" => OrderType::TrailingStop,
        _ => OrderType::Market,
    };

    let status = match info.status.to_lowercase().as_str() {
        "new" | "accepted" | "pending_new" => OrderStatus::Open,
        "partially_filled" => OrderStatus::PartiallyFilled,
        "filled" => OrderStatus::Filled,
        "canceled" | "cancelled" => OrderStatus::Cancelled,
        "rejected" => OrderStatus::Rejected,
        "expired" => OrderStatus::Expired,
        "pending_cancel" => OrderStatus::PendingCancel,
        _ => OrderStatus::Pending,
    };

    let quantity = info
        .qty
        .as_deref()
        .and_then(|q| q.parse::<Decimal>().ok())
        .unwrap_or_default();

    let filled_quantity = info
        .filled_qty
        .as_deref()
        .and_then(|q| q.parse::<Decimal>().ok())
        .unwrap_or_default();

    let remaining = (quantity - filled_quantity).max(Decimal::ZERO);

    let average_fill_price = info
        .filled_avg_price
        .as_deref()
        .and_then(|p| p.parse::<Decimal>().ok())
        .filter(|p| *p > Decimal::ZERO);

    let limit_price = info
        .limit_price
        .as_deref()
        .and_then(|p| p.parse::<Decimal>().ok())
        .filter(|p| *p > Decimal::ZERO);

    let stop_price = info
        .stop_price
        .as_deref()
        .and_then(|p| p.parse::<Decimal>().ok())
        .filter(|p| *p > Decimal::ZERO);

    let time_in_force = match info.time_in_force.to_lowercase().as_str() {
        "day" => TimeInForce::Day,
        "ioc" => TimeInForce::Ioc,
        "fok" => TimeInForce::Fok,
        _ => TimeInForce::Gtc,
    };

    let created_at = info
        .created_at
        .as_deref()
        .and_then(parse_alpaca_timestamp_to_datetime)
        .unwrap_or_else(Utc::now);

    let updated_at = info
        .updated_at
        .as_deref()
        .and_then(parse_alpaca_timestamp_to_datetime)
        .unwrap_or_else(Utc::now);

    Order {
        id: info.id.clone(),
        client_order_id: info.client_order_id.clone(),
        symbol: info.symbol.clone(),
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
        time_in_force,
        created_at,
        updated_at,
    }
}

/// Map an Alpaca trading event name to a gateway `OrderEventType`.
fn alpaca_event_to_order_event_type(event: &str) -> OrderEventType {
    match event {
        "new" | "accepted" => OrderEventType::OrderCreated,
        "fill" => OrderEventType::OrderFilled,
        "partial_fill" => OrderEventType::OrderPartiallyFilled,
        "canceled" | "cancelled" => OrderEventType::OrderCancelled,
        "rejected" => OrderEventType::OrderRejected,
        "expired" => OrderEventType::OrderExpired,
        // "replaced" and anything unrecognised
        _ => OrderEventType::OrderModified,
    }
}

/// Parse an Alpaca timestamp string to `DateTime<Utc>`.
///
/// Alpaca timestamps come in RFC 3339 format.
fn parse_alpaca_timestamp_to_datetime(s: &str) -> Option<DateTime<Utc>> {
    s.parse::<DateTime<Utc>>().ok()
}

/// Parse a trading stream event into a `WsMessage`.
fn parse_trading_event(update: &AlpacaTradingOrderUpdate) -> WsMessage {
    let order = alpaca_order_to_trading_order(&update.order);
    let event_type = alpaca_event_to_order_event_type(&update.event);
    let timestamp = update
        .timestamp
        .as_deref()
        .and_then(parse_alpaca_timestamp_to_datetime)
        .unwrap_or_else(Utc::now);

    WsMessage::Order {
        event: event_type,
        order,
        parent_order_id: None,
        timestamp,
    }
}

/// Parse a trading stream event into an `InternalTradingEvent` with position
/// context for the `EventRouter`.
///
/// Used when events need to flow through the internal event pipeline (e.g.,
/// for position synthesis from fill events with `position_qty`).
#[allow(dead_code)]
fn parse_internal_trading_event(
    update: &AlpacaTradingOrderUpdate,
    platform: TradingPlatform,
) -> InternalTradingEvent {
    let message = parse_trading_event(update);

    let context = update.position_qty.as_ref().map(|pq| {
        let position_qty = Decimal::from_str(pq).unwrap_or(Decimal::ZERO);
        OrderContext::new().with_position_qty(position_qty)
    });

    if let Some(ctx) = context {
        InternalTradingEvent::with_context(message, ctx, platform)
    } else {
        InternalTradingEvent::new(message, platform)
    }
}

// ============================================================================
// AlpacaWebSocketProvider Implementation
// ============================================================================

impl AlpacaWebSocketProvider {
    /// Create a new Alpaca WebSocket provider with the given feed type.
    ///
    /// # Arguments
    ///
    /// * `feed` - Feed type: `"iex"` (free) or `"sip"` (paid subscription)
    #[must_use]
    pub fn new(feed: impl Into<String>) -> Self {
        Self {
            feed: feed.into(),
            base_url: None,
            subscriptions: Arc::new(RwLock::new(SubscriptionState::default())),
            connection_state: Arc::new(RwLock::new(ConnectionState::default())),
            write_half: Arc::new(Mutex::new(None)),
            last_pong: Arc::new(RwLock::new(None)),
            ping_interval: 30,
            ping_timeout: 10,
            cancellation_token: CancellationToken::new(),
            trading_connection_state: Arc::new(RwLock::new(ConnectionState::default())),
            trading_stream_enabled: Arc::new(RwLock::new(false)),
            stored_config: Arc::new(RwLock::new(None)),
        }
    }

    /// Override the base URL (for testing with mock servers).
    #[must_use]
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self
    }

    /// Build the WebSocket URL for market data.
    ///
    /// Production: `wss://stream.data.alpaca.markets/v2/{feed}`
    /// Crypto:     `wss://stream.data.alpaca.markets/v1beta3/crypto/us`
    fn build_url(&self, is_crypto: bool) -> String {
        if let Some(ref base) = self.base_url {
            return base.clone();
        }

        if is_crypto {
            "wss://stream.data.alpaca.markets/v1beta3/crypto/us".to_string()
        } else {
            format!("wss://stream.data.alpaca.markets/v2/{}", self.feed)
        }
    }

    /// Check if the given feed is for crypto data.
    fn is_crypto_feed(symbols: &[String]) -> bool {
        symbols.iter().any(|s| {
            s.contains('/')
                || s.starts_with("BTC")
                || s.starts_with("ETH")
                || s.starts_with("SOL")
                || s.starts_with("DOGE")
                || s.ends_with("USD")
                || s.ends_with("USDT")
        })
    }

    /// Authenticate with the Alpaca market data WebSocket.
    async fn authenticate(
        write: &mut WsWriteHalf,
        credentials: &Credentials,
    ) -> Result<(), WebSocketError> {
        let auth = AlpacaAuthMessage {
            action: "auth".to_string(),
            key: credentials.api_key.expose_secret().clone(),
            secret: credentials.api_secret.expose_secret().clone(),
        };

        let json = serde_json::to_string(&auth)
            .map_err(|e| WebSocketError::SerializationFailed(e.to_string()))?;

        write
            .send(Message::Text(json.into()))
            .await
            .map_err(|e| WebSocketError::SendError(format!("Failed to send auth: {e}")))?;

        Ok(())
    }

    /// Send subscription message to the Alpaca market data WebSocket.
    async fn send_subscription(
        write: &mut WsWriteHalf,
        symbols: &[String],
        event_types: &[String],
        action: &str,
    ) -> Result<(), WebSocketError> {
        let normalized = symbols.to_vec();

        let mut trades = Vec::new();
        let mut quotes = Vec::new();
        let mut bars = Vec::new();

        for event_type in event_types {
            match event_type.as_str() {
                "quote" | "quotes" => quotes.clone_from(&normalized),
                "candle" | "candles" | "bar" | "bars" => bars.clone_from(&normalized),
                s if s.starts_with("candle_") => bars.clone_from(&normalized),
                "trade" | "trades" => trades.clone_from(&normalized),
                // Trading events handled by trading stream, not market data
                "order_update" | "trade_update" | "account_update" => {}
                other => {
                    warn!("Unknown Alpaca event type for subscription: {other}");
                }
            }
        }

        let sub = AlpacaSubscribeMessage {
            action: action.to_string(),
            trades,
            quotes,
            bars,
        };

        let json = serde_json::to_string(&sub)
            .map_err(|e| WebSocketError::SerializationFailed(e.to_string()))?;

        debug!("Sending Alpaca subscription: {json}");

        write
            .send(Message::Text(json.into()))
            .await
            .map_err(|e| WebSocketError::SendError(format!("Failed to send subscription: {e}")))?;

        Ok(())
    }

    /// Process a raw text message from the Alpaca market data WebSocket.
    ///
    /// Returns a list of `WsMessage` events (may be 0 or more per raw message,
    /// since Alpaca batches messages in JSON arrays).
    fn process_alpaca_message(text: &str, platform: TradingPlatform) -> Vec<WsMessage> {
        // Alpaca sends arrays of messages: [{"T":"q",...},{"T":"b",...}]
        let messages: Vec<serde_json::Value> = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => {
                debug!(
                    platform = ?platform,
                    "Failed to parse Alpaca message as array: {e}, text: {}",
                    &text[..text.len().min(200)]
                );
                return vec![];
            }
        };

        let mut events = Vec::new();

        for msg in messages {
            let msg_type = msg.get("T").and_then(serde_json::Value::as_str);

            match msg_type {
                Some("q") => {
                    if let Ok(quote_msg) = serde_json::from_value::<AlpacaQuoteMessage>(msg.clone())
                        && let Some(ws_msg) =
                            Self::convert_quote_to_ws_message(&quote_msg, platform)
                    {
                        events.push(ws_msg);
                    }
                }
                Some("b") => {
                    if let Ok(bar_msg) = serde_json::from_value::<AlpacaBarMessage>(msg.clone()) {
                        events.push(Self::convert_bar_to_ws_message(&bar_msg, platform));
                    }
                }
                Some("t") => {
                    // Trade messages - we convert to quotes using last trade price
                    if let Ok(trade_msg) = serde_json::from_value::<AlpacaTradeMessage>(msg.clone())
                    {
                        let price = Decimal::from_f64(trade_msg.price).unwrap_or_default();
                        let symbol = trade_msg.symbol.clone();
                        let timestamp = parse_alpaca_timestamp_to_datetime(&trade_msg.timestamp)
                            .unwrap_or_else(Utc::now);

                        events.push(WsMessage::quote(Quote {
                            symbol,
                            provider: alpaca_provider_name(platform).to_string(),
                            bid: price,
                            bid_size: Decimal::from_f64(trade_msg.size),
                            ask: price,
                            ask_size: None,
                            last: price,
                            volume: Decimal::from_f64(trade_msg.size),
                            timestamp,
                        }));
                    }
                }
                Some("success") => {
                    let action = msg.get("msg").and_then(serde_json::Value::as_str);
                    match action {
                        Some("connected") => {
                            debug!(platform = ?platform, "Alpaca WebSocket connected");
                        }
                        Some("authenticated") => {
                            info!(platform = ?platform, "Alpaca WebSocket authenticated");
                        }
                        _ => {
                            debug!(
                                platform = ?platform,
                                "Alpaca success message: {msg}"
                            );
                        }
                    }
                }
                Some("subscription") => {
                    debug!(platform = ?platform, "Alpaca subscription confirmation: {msg}");
                }
                Some("error") => {
                    let code = msg.get("code").and_then(serde_json::Value::as_i64);
                    let err_msg = msg
                        .get("msg")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown");

                    // Auth errors (401/403) are permanent
                    if matches!(code, Some(401 | 403)) {
                        error!(
                            platform = ?platform,
                            "Alpaca permanent auth error (code={code:?}): {err_msg}"
                        );
                    } else {
                        error!(
                            platform = ?platform,
                            "Alpaca WebSocket error (code={code:?}): {err_msg}"
                        );
                    }
                }
                _ => {
                    debug!(
                        platform = ?platform,
                        "Unknown Alpaca message type: {msg_type:?}"
                    );
                }
            }
        }

        events
    }

    /// Convert an Alpaca quote message to a `WsMessage::QuoteData`.
    fn convert_quote_to_ws_message(
        msg: &AlpacaQuoteMessage,
        platform: TradingPlatform,
    ) -> Option<WsMessage> {
        let bid = Decimal::from_f64(msg.bid_price)?;
        let ask = Decimal::from_f64(msg.ask_price)?;
        let mid = (bid + ask) / Decimal::from(2);

        let symbol = msg.symbol.clone();
        let timestamp = parse_alpaca_timestamp_to_datetime(&msg.timestamp).unwrap_or_else(Utc::now);

        Some(WsMessage::quote(Quote {
            symbol,
            provider: alpaca_provider_name(platform).to_string(),
            bid,
            bid_size: Decimal::from_f64(msg.bid_size),
            ask,
            ask_size: Decimal::from_f64(msg.ask_size),
            last: mid,
            volume: None,
            timestamp,
        }))
    }

    /// Convert an Alpaca bar message to a `WsMessage::Candle`.
    fn convert_bar_to_ws_message(msg: &AlpacaBarMessage, platform: TradingPlatform) -> WsMessage {
        let symbol = msg.symbol.clone();
        let timestamp = parse_alpaca_timestamp_to_datetime(&msg.timestamp).unwrap_or_else(Utc::now);

        WsMessage::candle(Bar {
            symbol,
            provider: alpaca_provider_name(platform).to_string(),
            timeframe: "1m".parse().unwrap_or_default(),
            timestamp,
            open: Decimal::from_f64(msg.open).unwrap_or_default(),
            high: Decimal::from_f64(msg.high).unwrap_or_default(),
            low: Decimal::from_f64(msg.low).unwrap_or_default(),
            close: Decimal::from_f64(msg.close).unwrap_or_default(),
            volume: Decimal::from_f64(msg.volume).unwrap_or_default(),
        })
    }

    /// Connect the internal trading stream and feed events into the provided
    /// sender. Returns immediately; spawns a background task for reading.
    async fn connect_internal_trading_stream(
        &self,
        credentials: &Credentials,
        platform: TradingPlatform,
        tx: mpsc::UnboundedSender<ProviderEvent>,
    ) -> Result<(), WebSocketError> {
        let url = self.trading_stream_url(platform);
        info!(platform = ?platform, %url, "Connecting to Alpaca trading stream");

        *self.trading_connection_state.write().await = ConnectionState::Connecting;

        let ws_stream = connect_with_retry(&url).await?;
        let (mut write, read) = ws_stream.split();

        // Authenticate
        let auth = TradingAuthMessage {
            action: "authenticate".to_string(),
            data: TradingAuthData {
                key_id: credentials.api_key.expose_secret().clone(),
                secret_key: credentials.api_secret.expose_secret().clone(),
            },
        };

        let json = serde_json::to_string(&auth)
            .map_err(|e| WebSocketError::SerializationFailed(e.to_string()))?;

        write
            .send(Message::Text(json.into()))
            .await
            .map_err(|e| WebSocketError::SendError(format!("Failed to send trading auth: {e}")))?;

        // Subscribe to trade updates
        let listen = TradingListenMessage {
            action: "listen".to_string(),
            data: TradingListenData {
                streams: vec!["trade_updates".to_string()],
            },
        };

        let json = serde_json::to_string(&listen)
            .map_err(|e| WebSocketError::SerializationFailed(e.to_string()))?;

        write.send(Message::Text(json.into())).await.map_err(|e| {
            WebSocketError::SendError(format!("Failed to send trading listen: {e}"))
        })?;

        *self.trading_connection_state.write().await = ConnectionState::Authenticated;
        *self.trading_stream_enabled.write().await = true;

        spawn_trading_stream_reader(
            read,
            tx,
            platform,
            self.cancellation_token.clone(),
            self.trading_connection_state.clone(),
        );

        Ok(())
    }

    /// Connect to the trading stream (convenience wrapper).
    async fn connect_trading_stream(
        &self,
        credentials: &Credentials,
        platform: TradingPlatform,
        tx: mpsc::UnboundedSender<ProviderEvent>,
    ) -> Result<(), WebSocketError> {
        self.connect_internal_trading_stream(credentials, platform, tx)
            .await
    }

    /// Wait for the Alpaca market data authentication response.
    ///
    /// Reads messages until an "authenticated" response is received, or returns
    /// an error on auth failure or timeout.
    async fn wait_for_auth_response(
        read: &mut futures_util::stream::SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    ) -> Result<(), WebSocketError> {
        let auth_timeout = Duration::from_secs(10);
        let auth_result = tokio::time::timeout(auth_timeout, async {
            while let Some(msg_result) = read.next().await {
                match msg_result {
                    Ok(Message::Text(text)) => {
                        let text_str: &str = &text;
                        if text_str.contains("\"authenticated\"") {
                            return Ok(());
                        }
                        if text_str.contains("\"error\"") {
                            if text_str.contains("401") || text_str.contains("403") {
                                return Err(WebSocketError::PermanentAuthError(format!(
                                    "Alpaca auth failed: {text}"
                                )));
                            }
                            return Err(WebSocketError::ConnectionFailed(format!(
                                "Alpaca auth error: {text}"
                            )));
                        }
                    }
                    Err(e) => {
                        return Err(WebSocketError::ConnectionFailed(format!(
                            "WebSocket error during auth: {e}"
                        )));
                    }
                    _ => {}
                }
            }
            Err(WebSocketError::ConnectionClosed(
                "Connection closed during auth".to_string(),
            ))
        })
        .await;

        match auth_result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(WebSocketError::ConnectionFailed(
                "Alpaca auth timed out after 10s".to_string(),
            )),
        }
    }

    /// Get the trading stream WebSocket URL for the given platform.
    fn trading_stream_url(&self, platform: TradingPlatform) -> String {
        if let Some(ref base) = self.base_url {
            return base.clone();
        }

        if platform.is_paper() {
            "wss://paper-api.alpaca.markets/stream".to_string()
        } else {
            "wss://api.alpaca.markets/stream".to_string()
        }
    }
}

/// Spawn a background task that reads trading stream messages and forwards
/// parsed order events to the provided channel.
fn spawn_trading_stream_reader(
    mut read: futures_util::stream::SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    tx: mpsc::UnboundedSender<ProviderEvent>,
    platform: TradingPlatform,
    cancel_token: CancellationToken,
    trading_state: Arc<RwLock<ConnectionState>>,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = cancel_token.cancelled() => {
                    *trading_state.write().await = ConnectionState::Disconnected;
                    break;
                }
                msg_result = read.next() => {
                    match msg_result {
                        Some(Ok(Message::Text(text))) => {
                            process_trading_stream_text(
                                &text, platform, &tx,
                            );
                        }
                        Some(Ok(Message::Close(frame))) => {
                            info!(
                                platform = ?platform,
                                "Trading stream closed: {frame:?}"
                            );
                            *trading_state.write().await = ConnectionState::Disconnected;
                            let _ = tx.send(WsMessage::Connection {
                                event: ConnectionEventType::BrokerDisconnected,
                                error: Some("Trading stream closed".to_string()),
                                broker: Some(platform.to_string()),
                                gap_duration_ms: None,
                                timestamp: Utc::now(),
                            }.into());
                            return;
                        }
                        Some(Err(e)) => {
                            error!(
                                platform = ?platform,
                                "Trading stream error: {e}"
                            );
                            *trading_state.write().await =
                                ConnectionState::Error(format!("{e}"));
                            let _ = tx.send(WsMessage::Connection {
                                event: ConnectionEventType::BrokerDisconnected,
                                error: Some(format!("Trading stream error: {e}")),
                                broker: Some(platform.to_string()),
                                gap_duration_ms: None,
                                timestamp: Utc::now(),
                            }.into());
                            return;
                        }
                        None => {
                            *trading_state.write().await = ConnectionState::Disconnected;
                            return;
                        }
                        _ => {}
                    }
                }
            }
        }
    });
}

/// Process a single text message from the Alpaca trading stream.
fn process_trading_stream_text(
    text: &str,
    platform: TradingPlatform,
    tx: &mpsc::UnboundedSender<ProviderEvent>,
) {
    match serde_json::from_str::<AlpacaTradingStreamMessage>(text) {
        Ok(stream_msg) => {
            if stream_msg.stream == "trade_updates" {
                if let Some(data) = stream_msg.data {
                    match serde_json::from_value::<AlpacaTradingOrderUpdate>(data) {
                        Ok(update) => {
                            let ws_msg = parse_trading_event(&update);
                            let _ = tx.send(ws_msg.into());
                        }
                        Err(e) => {
                            warn!(
                                platform = ?platform,
                                "Failed to parse trading order update: {e}"
                            );
                        }
                    }
                }
            } else if stream_msg.stream == "authorization" {
                debug!(
                    platform = ?platform,
                    "Trading stream auth response: {:?}", stream_msg.data
                );
            } else if stream_msg.stream == "listening" {
                debug!(
                    platform = ?platform,
                    "Trading stream listening: {:?}", stream_msg.data
                );
            } else {
                debug!(
                    platform = ?platform,
                    "Unknown trading stream: {}", stream_msg.stream
                );
            }
        }
        Err(e) => {
            debug!(
                platform = ?platform,
                "Non-standard trading message: {e}, text: {}",
                &text[..text.len().min(200)]
            );
        }
    }
}

/// Spawn a background task that reads market data WebSocket messages and
/// forwards parsed events to the provided channel.
fn spawn_market_data_reader(
    mut read: futures_util::stream::SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    tx: mpsc::UnboundedSender<ProviderEvent>,
    platform: TradingPlatform,
    cancel_token: CancellationToken,
    connection_state: Arc<RwLock<ConnectionState>>,
    last_pong: Arc<RwLock<Option<SystemTime>>>,
) {
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
                            let text_str: &str = &text;
                            for event in AlpacaWebSocketProvider::process_alpaca_message(text_str, platform) {
                                if tx.send(event.into()).is_err() {
                                    return;
                                }
                            }
                        }
                        Some(Ok(Message::Pong(_))) => {
                            *last_pong.write().await = Some(SystemTime::now());
                        }
                        Some(Ok(Message::Close(frame))) => {
                            info!(
                                platform = ?platform,
                                "Market data WebSocket closed: {frame:?}"
                            );
                            *connection_state.write().await = ConnectionState::Disconnected;
                            let _ = tx.send(WsMessage::Connection {
                                event: ConnectionEventType::BrokerDisconnected,
                                error: Some("Market data stream closed".to_string()),
                                broker: Some(platform.to_string()),
                                gap_duration_ms: None,
                                timestamp: Utc::now(),
                            }.into());
                            return;
                        }
                        Some(Err(e)) => {
                            error!(
                                platform = ?platform,
                                "Market data WebSocket error: {e}"
                            );
                            *connection_state.write().await =
                                ConnectionState::Error(format!("{e}"));
                            let _ = tx.send(WsMessage::Connection {
                                event: ConnectionEventType::BrokerDisconnected,
                                error: Some(format!("WebSocket error: {e}")),
                                broker: Some(platform.to_string()),
                                gap_duration_ms: None,
                                timestamp: Utc::now(),
                            }.into());
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
}

// ============================================================================
// WebSocketProvider Trait Implementation
// ============================================================================

#[async_trait]
impl WebSocketProvider for AlpacaWebSocketProvider {
    async fn connect(&self, config: ProviderConfig) -> Result<EventStream, WebSocketError> {
        // Store config for reconnection
        *self.stored_config.write().await = Some(config.clone());

        let credentials = config.credentials.as_ref().ok_or_else(|| {
            WebSocketError::ConfigError("Alpaca requires credentials for WebSocket".to_string())
        })?;

        let platform = config.platform;
        let is_crypto = Self::is_crypto_feed(&config.symbols);
        let url = self.build_url(is_crypto);

        info!(
            platform = ?platform,
            %url,
            symbols = ?config.symbols,
            "Connecting to Alpaca market data WebSocket"
        );

        *self.connection_state.write().await = ConnectionState::Connecting;

        // Connect to market data WebSocket
        let ws_stream = connect_with_retry(&url).await?;
        let (mut write, mut read) = ws_stream.split();

        *self.connection_state.write().await = ConnectionState::Connected;

        // Authenticate
        Self::authenticate(&mut write, credentials).await?;

        // Wait for auth response
        Self::wait_for_auth_response(&mut read).await?;
        *self.connection_state.write().await = ConnectionState::Authenticated;

        // Subscribe to requested symbols
        Self::send_subscription(
            &mut write,
            &config.symbols,
            &config.event_types,
            "subscribe",
        )
        .await?;

        // Update subscription state
        {
            let mut subs = self.subscriptions.write().await;
            subs.symbols.clone_from(&config.symbols);
            subs.event_types.clone_from(&config.event_types);
        }
        *self.connection_state.write().await = ConnectionState::Subscribed;

        // Store write half
        *self.write_half.lock().await = Some(write);

        // Create event channel
        let (tx, rx) = mpsc::unbounded_channel();

        // Spawn market data read task
        spawn_market_data_reader(
            read,
            tx.clone(),
            platform,
            self.cancellation_token.clone(),
            self.connection_state.clone(),
            self.last_pong.clone(),
        );

        // Connect trading stream if event types include order/trade updates
        let needs_trading = config.event_types.iter().any(|e| {
            matches!(
                e.as_str(),
                "order_update" | "trade_update" | "account_update"
            )
        });

        if needs_trading
            && let Err(e) = self.connect_trading_stream(credentials, platform, tx).await
        {
            warn!(
                platform = ?platform,
                "Failed to connect trading stream: {e}"
            );
            // Market data still works -- log but don't fail
        }

        info!(platform = ?platform, "Alpaca WebSocket provider connected");
        Ok(rx)
    }

    async fn subscribe(
        &self,
        symbols: Vec<String>,
        event_types: Vec<String>,
    ) -> Result<(), WebSocketError> {
        let mut write_guard = self.write_half.lock().await;
        let write = write_guard
            .as_mut()
            .ok_or_else(|| WebSocketError::ConnectionClosed("Not connected".to_string()))?;

        Self::send_subscription(write, &symbols, &event_types, "subscribe").await?;
        drop(write_guard);

        // Update subscription state
        let mut subs = self.subscriptions.write().await;
        for symbol in &symbols {
            if !subs.symbols.contains(symbol) {
                subs.symbols.push(symbol.clone());
            }
        }
        for event_type in &event_types {
            if !subs.event_types.contains(event_type) {
                subs.event_types.push(event_type.clone());
            }
        }
        drop(subs);

        Ok(())
    }

    async fn unsubscribe(&self, symbols: Vec<String>) -> Result<(), WebSocketError> {
        let event_types = {
            let subs = self.subscriptions.read().await;
            subs.event_types.clone()
        };

        let mut write_guard = self.write_half.lock().await;
        let write = write_guard
            .as_mut()
            .ok_or_else(|| WebSocketError::ConnectionClosed("Not connected".to_string()))?;

        Self::send_subscription(write, &symbols, &event_types, "unsubscribe").await?;
        drop(write_guard);

        // Update subscription state
        let mut subs = self.subscriptions.write().await;
        subs.symbols.retain(|s| !symbols.contains(s));
        drop(subs);

        Ok(())
    }

    async fn handle_ack(&self, ack: EventAckMessage) -> Result<(), WebSocketError> {
        debug!(
            "Received ACK for {} events (informational only for live trading)",
            ack.events_processed.len()
        );
        Ok(())
    }

    async fn disconnect(&self) -> Result<(), WebSocketError> {
        self.cancellation_token.cancel();
        *self.connection_state.write().await = ConnectionState::Disconnected;
        *self.trading_connection_state.write().await = ConnectionState::Disconnected;
        *self.write_half.lock().await = None;
        *self.trading_stream_enabled.write().await = false;

        info!("Alpaca WebSocket provider disconnected");
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
        *self.trading_connection_state.write().await = ConnectionState::Disconnected;
        *self.write_half.lock().await = None;

        // Reconnect
        self.connect(config).await
    }
}

// ============================================================================
// Module-Level Helpers
// ============================================================================

/// Connect to a WebSocket URL with retry + exponential backoff.
async fn connect_with_retry(
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
                    "Alpaca WebSocket connection failed (attempt {retry_count}/{MAX_RETRIES}), \
                     retrying in {backoff:?}: {e}"
                );
                sleep(backoff).await;
            }
        }
    }
}

/// Provider name string for an Alpaca platform.
const fn alpaca_provider_name(platform: TradingPlatform) -> &'static str {
    if platform.is_paper() {
        "alpaca-paper"
    } else {
        "alpaca-live"
    }
}
