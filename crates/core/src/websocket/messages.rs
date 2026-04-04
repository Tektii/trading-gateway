//! WebSocket message types for real-time trading events.
//!
//! This module defines all messages exchanged over WebSocket connections
//! between clients and the trading API, plus proxy-specific helper types
//! for internal event routing and error handling.
//!
//! # Message Format
//!
//! All messages use JSON with internally-tagged representation:
//!
//! ```json
//! {"type": "order", "event": "ORDER_FILLED", "order": {...}}
//! {"type": "ping", "timestamp": "2025-01-01T00:00:00Z"}
//! ```
//!
//! # Event Broadcasts
//!
//! The server broadcasts trading events to all connected clients:
//! - `order` - Order state changes (created, filled, cancelled)
//! - `position` - Position updates (opened, modified, closed)
//! - `account` - Account balance/margin updates
//! - `connection` - Connection state changes
//! - `rate_limit` - Rate limit notifications
//! - `error` - Error notifications
//!
//! # Subscription Configuration
//!
//! Event subscriptions are configured at startup via environment variables
//! (e.g., `SUBSCRIPTIONS`), not through runtime WebSocket messages.

#![allow(dead_code)]

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;
use validator::Validate;

use crate::models::{
    Account, Bar, Order, OrderStatus, OrderType, Position, Quote, Side, TimeInForce, Timeframe,
    Trade, TradingPlatform,
};

// ============================================================================
// Server -> Client Message Payloads
// ============================================================================

/// Server ping payload.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub struct PingPayload {
    /// Server timestamp.
    pub timestamp: DateTime<Utc>,
}

// ============================================================================
// Trading Event Types
// ============================================================================

/// Order event types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderEventType {
    /// New order was created.
    OrderCreated,
    /// Order was modified.
    OrderModified,
    /// Order was cancelled.
    OrderCancelled,
    /// Order was rejected.
    OrderRejected,
    /// Order was completely filled.
    OrderFilled,
    /// Order was partially filled.
    OrderPartiallyFilled,
    /// Order expired (e.g., DAY order after market close).
    OrderExpired,
    /// Bracket order was created (entry + SL/TP).
    BracketOrderCreated,
    /// Bracket order was modified.
    BracketOrderModified,
}

impl OrderEventType {
    /// Stable snake_case label for Prometheus metrics.
    #[must_use]
    pub const fn metric_label(&self) -> &'static str {
        match self {
            Self::OrderCreated => "created",
            Self::OrderModified => "modified",
            Self::OrderCancelled => "cancelled",
            Self::OrderRejected => "rejected",
            Self::OrderFilled => "filled",
            Self::OrderPartiallyFilled => "partially_filled",
            Self::OrderExpired => "expired",
            Self::BracketOrderCreated => "bracket_created",
            Self::BracketOrderModified => "bracket_modified",
        }
    }
}

/// Position event types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PositionEventType {
    /// New position was opened.
    PositionOpened,
    /// Position was modified (quantity changed, partial close).
    PositionModified,
    /// Position was fully closed.
    PositionClosed,
}

/// Account event types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AccountEventType {
    /// Account balance was updated.
    BalanceUpdated,
    /// Margin usage warning (approaching limit).
    MarginWarning,
    /// Margin call (positions at risk of liquidation).
    MarginCall,
}

/// Connection event types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ConnectionEventType {
    /// Successfully connected to provider.
    Connected,
    /// Connection is being closed gracefully.
    Disconnecting,
    /// Attempting to reconnect after disconnect.
    Reconnecting,
    /// Broker WebSocket connection was lost.
    BrokerDisconnected,
    /// Broker WebSocket connection was restored after a gap.
    BrokerReconnected,
    /// Broker WebSocket reconnection failed permanently (gave up after max retries).
    BrokerConnectionFailed,
}

// TODO: Add `reason` field to STALE events (e.g., Disconnect, Timeout, ExchangeHalt)
// to let strategies differentiate feed loss from exchange halts.

/// Data staleness event types.
///
/// Sent per-instrument when market data becomes stale (broker feed lost) or
/// fresh (first live tick received after reconnect).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DataStalenessEventType {
    /// Market data is stale — broker feed lost, prices may be outdated.
    Stale,
    /// Market data is fresh — first live tick received after reconnect.
    Fresh,
}

/// Rate limit event types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RateLimitEventType {
    /// Approaching rate limit threshold.
    RateLimitWarning,
    /// Rate limit reached, requests will be throttled/rejected.
    RateLimitHit,
}

impl RateLimitEventType {
    /// Stable snake_case label for Prometheus metrics.
    #[must_use]
    pub const fn metric_label(&self) -> &'static str {
        match self {
            Self::RateLimitWarning => "warning",
            Self::RateLimitHit => "hit",
        }
    }
}

/// Trade event types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TradeEventType {
    /// New trade execution (order fill).
    TradeFilled,
}

/// WebSocket error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum WsErrorCode {
    /// Invalid message format.
    InvalidMessage,
    /// Internal server error.
    InternalError,
    /// Position is unprotected due to exit order placement failure.
    ///
    /// Sent when SL/TP orders fail to place after a fill event, leaving
    /// the position without stop-loss or take-profit protection. This can
    /// occur when the circuit breaker trips after repeated failures.
    PositionUnprotected,
    /// Position was force-liquidated by the exchange.
    ///
    /// Sent when a leveraged position (margin or futures) is automatically
    /// closed by the exchange due to insufficient margin. The strategy
    /// should treat this position as closed and stop managing it.
    Liquidation,
    /// OCO double-exit detected: both SL and TP filled at the provider.
    ///
    /// Sent when both legs of an OCO pair fill before the gateway can cancel
    /// the sibling. The position has been exited at more than the intended
    /// quantity. The strategy should reconcile its position state and may
    /// need to place a corrective order.
    OcoDoubleExit,
}

// ============================================================================
// Unified Message Enum
// ============================================================================

/// All possible WebSocket messages.
///
/// Uses serde's internally tagged representation for clean JSON:
/// ```json
/// {"type": "order", "event": "ORDER_FILLED", "order": {...}}
/// {"type": "ping", "timestamp": "2025-01-01T00:00:00Z"}
/// ```
///
/// # Client -> Server Messages
///
/// - `Pong` - Response to server ping (heartbeat)
/// - `EventAck` - Acknowledge processed events (required for simulation, optional for live)
///
/// # Server -> Client Messages
///
/// - `Ping` - Server heartbeat
/// - `Order` - Order state change
/// - `Position` - Position state change
/// - `Account` - Account state change
/// - `Connection` - Connection state change
/// - `RateLimit` - Rate limit notification
/// - `Error` - Error notification
///
/// # Subscription Configuration
///
/// Note: There are no Subscribe/Unsubscribe messages. Event subscriptions
/// are configured at startup via environment variables (e.g., `SUBSCRIPTIONS`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsMessage {
    // === Client -> Server ===
    /// Response to server ping.
    Pong,

    /// Event acknowledgment from strategy.
    ///
    /// Strategies send this message to acknowledge they have processed events.
    /// This is **required** for simulation mode (controls time progression) and
    /// **informational-only** for live trading.
    ///
    /// # Example
    /// ```json
    /// {
    ///   "type": "event_ack",
    ///   "correlation_id": "550e8400-e29b-41d4-a716-446655440000",
    ///   "events_processed": ["evt_123", "evt_124"],
    ///   "timestamp": 1700000000000
    /// }
    /// ```
    EventAck {
        /// Unique correlation ID for tracking this acknowledgment.
        correlation_id: String,
        /// List of event IDs that were processed by the strategy.
        events_processed: Vec<String>,
        /// Unix timestamp (milliseconds) when events were processed.
        timestamp: u64,
    },

    // === Server -> Client ===
    /// Server heartbeat ping.
    Ping {
        /// Server timestamp.
        timestamp: DateTime<Utc>,
    },

    /// Order state change.
    Order {
        /// Type of order event.
        event: OrderEventType,
        /// Current state of the order.
        order: Order,
        /// Parent order ID for bracket/OCO child orders.
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_order_id: Option<String>,
        /// Event timestamp.
        timestamp: DateTime<Utc>,
    },

    /// Position state change.
    Position {
        /// Type of position event.
        event: PositionEventType,
        /// Current state of the position.
        position: Position,
        /// Event timestamp.
        timestamp: DateTime<Utc>,
    },

    /// Account state change.
    Account {
        /// Type of account event.
        event: AccountEventType,
        /// Current account state.
        account: Account,
        /// Event timestamp.
        timestamp: DateTime<Utc>,
    },

    /// Candle (OHLCV bar) data.
    Candle {
        /// The bar data (includes symbol, provider, timeframe, OHLCV).
        bar: Bar,
        /// Event timestamp (when event was emitted).
        timestamp: DateTime<Utc>,
    },

    /// Quote (bid/ask) data.
    #[serde(rename = "quote")]
    QuoteData {
        /// The quote data (includes symbol, provider, bid/ask/last).
        quote: Quote,
        /// Event timestamp (when event was emitted).
        timestamp: DateTime<Utc>,
    },

    /// Trade execution event.
    Trade {
        /// Type of trade event.
        event: TradeEventType,
        /// The executed trade.
        trade: Trade,
        /// Event timestamp.
        timestamp: DateTime<Utc>,
    },

    /// Connection state change.
    Connection {
        /// Type of connection event.
        event: ConnectionEventType,
        /// Error message if disconnecting/reconnecting due to error.
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// Broker/platform name (e.g., "alpaca-paper"). Present for broker events.
        #[serde(skip_serializing_if = "Option::is_none")]
        broker: Option<String>,
        /// Duration of the connection gap in milliseconds (for `BrokerReconnected`).
        #[serde(skip_serializing_if = "Option::is_none")]
        gap_duration_ms: Option<u64>,
        /// Event timestamp.
        timestamp: DateTime<Utc>,
    },

    /// Per-instrument data staleness notification.
    ///
    /// Sent when market data becomes stale (broker disconnect) or fresh
    /// (first tick received after reconnect). Strategies should pause
    /// trading decisions for stale instruments.
    DataStaleness {
        /// Whether data became stale or fresh.
        event: DataStalenessEventType,
        /// Affected instrument symbols.
        symbols: Vec<String>,
        /// When the data became stale. Present on both Stale (the moment it
        /// happened) and Fresh (for gap duration calculation) events.
        #[serde(skip_serializing_if = "Option::is_none")]
        stale_since: Option<DateTime<Utc>>,
        /// Broker/platform name.
        #[serde(skip_serializing_if = "Option::is_none")]
        broker: Option<String>,
        /// Event timestamp.
        timestamp: DateTime<Utc>,
    },

    /// Rate limit notification.
    RateLimit {
        /// Type of rate limit event.
        event: RateLimitEventType,
        /// Number of requests remaining in current window.
        requests_remaining: u32,
        /// When the rate limit window resets.
        reset_at: DateTime<Utc>,
        /// Event timestamp.
        timestamp: DateTime<Utc>,
    },

    /// Error notification.
    Error {
        /// Error code.
        code: WsErrorCode,
        /// Human-readable error message.
        message: String,
        /// Additional error details.
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<serde_json::Value>,
        /// Event timestamp.
        timestamp: DateTime<Utc>,
    },

    /// Internal close signal — not serialized to JSON.
    /// Intercepted in the send loop to emit a WebSocket close frame.
    #[serde(skip)]
    Close {
        /// WebSocket close status code (e.g., 1001 = Going Away).
        code: u16,
        /// Human-readable reason string.
        reason: String,
    },
}

// ============================================================================
// Helper Methods
// ============================================================================

impl WsMessage {
    /// Create a pong message.
    #[must_use]
    pub const fn pong() -> Self {
        Self::Pong
    }

    /// Create a ping message with current timestamp.
    #[must_use]
    pub fn ping() -> Self {
        Self::Ping {
            timestamp: Utc::now(),
        }
    }

    /// Create an error message.
    #[must_use]
    pub fn error(code: WsErrorCode, message: impl Into<String>) -> Self {
        Self::Error {
            code,
            message: message.into(),
            details: None,
            timestamp: Utc::now(),
        }
    }

    /// Create an error message with details.
    #[must_use]
    pub fn error_with_details(
        code: WsErrorCode,
        message: impl Into<String>,
        details: serde_json::Value,
    ) -> Self {
        Self::Error {
            code,
            message: message.into(),
            details: Some(details),
            timestamp: Utc::now(),
        }
    }

    /// Create a candle message from bar data.
    #[must_use]
    pub fn candle(bar: Bar) -> Self {
        Self::Candle {
            bar,
            timestamp: Utc::now(),
        }
    }

    /// Create a quote message from quote data.
    #[must_use]
    pub fn quote(quote: Quote) -> Self {
        Self::QuoteData {
            quote,
            timestamp: Utc::now(),
        }
    }

    /// Create a trade message from trade data.
    #[must_use]
    pub fn trade(trade: Trade) -> Self {
        Self::Trade {
            event: TradeEventType::TradeFilled,
            trade,
            timestamp: Utc::now(),
        }
    }

    /// Create an event acknowledgment message.
    ///
    /// Used by strategies to acknowledge they have processed events.
    /// Required for simulation mode, informational for live trading.
    #[must_use]
    pub fn event_ack(events_processed: Vec<String>) -> Self {
        Self::EventAck {
            correlation_id: Uuid::new_v4().to_string(),
            events_processed,
            timestamp: u64::try_from(Utc::now().timestamp_millis()).unwrap_or(0),
        }
    }

    /// Check if this is a client-to-server message.
    ///
    /// Client messages are:
    /// - `Pong` - Heartbeat response
    /// - `EventAck` - Event processing acknowledgment
    #[must_use]
    pub const fn is_client_message(&self) -> bool {
        matches!(self, Self::Pong | Self::EventAck { .. })
    }

    /// Check if this is a server-to-client message.
    #[must_use]
    pub const fn is_server_message(&self) -> bool {
        !self.is_client_message()
    }

    /// Create a broker disconnected event.
    #[must_use]
    pub fn broker_disconnected(platform: TradingPlatform) -> Self {
        Self::Connection {
            event: ConnectionEventType::BrokerDisconnected,
            error: None,
            broker: Some(platform.to_string()),
            gap_duration_ms: None,
            timestamp: Utc::now(),
        }
    }

    /// Create a broker reconnected event with gap duration.
    #[must_use]
    pub fn broker_reconnected(platform: TradingPlatform, gap: std::time::Duration) -> Self {
        Self::Connection {
            event: ConnectionEventType::BrokerReconnected,
            error: None,
            broker: Some(platform.to_string()),
            gap_duration_ms: Some(u64::try_from(gap.as_millis()).unwrap_or(u64::MAX)),
            timestamp: Utc::now(),
        }
    }

    /// Create a broker connection failed event (gave up after max retries).
    #[must_use]
    pub fn broker_connection_failed(platform: TradingPlatform) -> Self {
        Self::Connection {
            event: ConnectionEventType::BrokerConnectionFailed,
            error: None,
            broker: Some(platform.to_string()),
            gap_duration_ms: None,
            timestamp: Utc::now(),
        }
    }

    /// Create a data stale event (instruments lost live feed).
    #[must_use]
    pub fn data_stale(platform: TradingPlatform, symbols: Vec<String>) -> Self {
        let now = Utc::now();
        Self::DataStaleness {
            event: DataStalenessEventType::Stale,
            symbols,
            stale_since: Some(now),
            broker: Some(platform.to_string()),
            timestamp: now,
        }
    }

    /// Create a data fresh event (instrument received first tick after reconnect).
    #[must_use]
    pub fn data_fresh(
        platform: TradingPlatform,
        symbol: impl Into<String>,
        stale_since: DateTime<Utc>,
    ) -> Self {
        Self::DataStaleness {
            event: DataStalenessEventType::Fresh,
            symbols: vec![symbol.into()],
            stale_since: Some(stale_since),
            broker: Some(platform.to_string()),
            timestamp: Utc::now(),
        }
    }
}

// ============================================================================
// Event enum for subscription routing
// ============================================================================

/// Event types for subscription routing.
///
/// Represents all possible event types that can be subscribed to via WebSocket.
/// Events are parsed from and serialized to human-readable string format.
///
/// # Variants
///
/// ## Market Data
/// - [`Event::Candle`] - OHLCV bar data for specific timeframe
/// - [`Event::Quote`] - Real-time bid/ask/last prices
/// - [`Event::OptionGreeks`] - Option Greeks calculations
///
/// ## Trading
/// - [`Event::OrderUpdate`] - Order state changes
/// - [`Event::PositionUpdate`] - Position state changes
/// - [`Event::AccountUpdate`] - Account balance/margin changes
/// - [`Event::TradeUpdate`] - Trade execution notifications
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum Event {
    /// Candlestick (OHLCV) market data event with specific timeframe.
    ///
    /// Serializes to `candle_<timeframe>` format (e.g., `candle_1m`, `candle_1h`).
    Candle(Timeframe),

    /// Real-time quote (bid/ask/last) data.
    Quote,

    /// Option Greeks calculation event.
    OptionGreeks,

    /// Order status update event.
    OrderUpdate,

    /// Position state change event.
    PositionUpdate,

    /// Account balance/margin update event.
    AccountUpdate,

    /// Trade execution event.
    TradeUpdate,
}

impl std::str::FromStr for Event {
    type Err = EventParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Candle events use "candle_<timeframe>" format (e.g., "candle_1m", "candle_5m")
        // This format enables wildcard filtering like "candle_*" to match all timeframes
        if let Some(timeframe_str) = s.strip_prefix("candle_") {
            let tf: Timeframe = timeframe_str
                .parse()
                .map_err(|_| EventParseError::InvalidTimeframe(timeframe_str.to_string()))?;
            return Ok(Self::Candle(tf));
        }

        match s {
            "quote" => Ok(Self::Quote),
            "option_greeks" => Ok(Self::OptionGreeks),
            "order_update" => Ok(Self::OrderUpdate),
            "position_update" => Ok(Self::PositionUpdate),
            "account_update" => Ok(Self::AccountUpdate),
            "trade_update" => Ok(Self::TradeUpdate),
            _ => Err(EventParseError::InvalidEvent(s.to_string())),
        }
    }
}

impl std::fmt::Display for Event {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // Use "candle_<timeframe>" format for wildcard filtering support
            Self::Candle(timeframe) => write!(f, "candle_{timeframe}"),
            Self::Quote => write!(f, "quote"),
            Self::OptionGreeks => write!(f, "option_greeks"),
            Self::OrderUpdate => write!(f, "order_update"),
            Self::PositionUpdate => write!(f, "position_update"),
            Self::AccountUpdate => write!(f, "account_update"),
            Self::TradeUpdate => write!(f, "trade_update"),
        }
    }
}

/// Error type for event parsing failures.
#[derive(Debug, thiserror::Error)]
pub enum EventParseError {
    /// Invalid event name that doesn't match any known event type.
    #[error(
        "Invalid event: '{0}'. Valid events: candle_<timeframe>, quote, option_greeks, order_update, position_update, account_update, trade_update"
    )]
    InvalidEvent(String),

    /// Invalid timeframe string for candle events.
    #[error(
        "Invalid timeframe: '{0}'. Valid timeframes: 1m, 2m, 5m, 10m, 15m, 30m, 1h, 2h, 4h, 12h, 1d, 1w"
    )]
    InvalidTimeframe(String),
}

// ============================================================================
// Proxy-specific helper functions
// ============================================================================

/// Create a synthetic Order object for proxy-generated events.
///
/// This is used when the `EventRouter` needs to broadcast events for SL/TP orders
/// that may not have full order data from the provider (e.g., cancelled before placement).
///
/// # Arguments
///
/// * `id` - Order ID (actual order ID or placeholder ID)
/// * `symbol` - Trading symbol
/// * `side` - Order side
/// * `status` - Order status
/// * `quantity` - Total order quantity
/// * `filled_quantity` - Filled quantity
#[must_use]
pub fn synthetic_order(
    id: impl Into<String>,
    symbol: impl Into<String>,
    side: Side,
    status: OrderStatus,
    quantity: Decimal,
    filled_quantity: Decimal,
) -> Order {
    let now = Utc::now();
    let remaining = quantity - filled_quantity;

    Order {
        id: id.into(),
        client_order_id: None,
        symbol: symbol.into(),
        side,
        order_type: OrderType::Market, // Default for synthetic orders
        quantity,
        filled_quantity,
        remaining_quantity: remaining.max(Decimal::ZERO),
        limit_price: None,
        stop_price: None,
        stop_loss: None,
        take_profit: None,
        trailing_distance: None,
        trailing_type: None,
        average_fill_price: None,
        status,
        reject_reason: None,
        position_id: None,
        reduce_only: None,
        post_only: None,
        hidden: None,
        display_quantity: None,
        oco_group_id: None,
        correlation_id: None,
        time_in_force: TimeInForce::Gtc,
        created_at: now,
        updated_at: now,
    }
}

// ============================================================================
// Proxy-specific types for SL/TP and error handling
// ============================================================================

/// Severity level of SL/TP placement failure.
///
/// Indicates how critical the failure is from a risk management perspective.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureSeverity {
    /// Stop-loss failed - position is unprotected from losses.
    Critical,
    /// Take-profit failed - position still has SL protection.
    Warning,
}

/// Suggested action for handling SL/TP placement failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuggestedAction {
    /// Non-retryable error requiring manual intervention.
    ManualInterventionRequired,
    /// Transient error that may resolve with retry.
    RetryPossible,
    /// Critical failure requiring immediate position closure.
    ClosePositionImmediately,
}

/// Serde helper: serialize `Option<Decimal>` as a JSON float for wire compatibility.
/// The workspace default (`serde-with-str`) serializes Decimal as strings, but
/// `OrderContext` fields were previously `f64` and clients expect JSON numbers.
mod decimal_float_option {
    use rust_decimal::Decimal;
    use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
    use serde::{self, Deserialize, Deserializer, Serializer};

    #[allow(clippy::ref_option)]
    pub fn serialize<S>(value: &Option<Decimal>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(d) => {
                let f = d.to_f64().ok_or_else(|| {
                    serde::ser::Error::custom("Decimal value cannot be represented as f64")
                })?;
                serializer.serialize_f64(f)
            }
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Decimal>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<f64>::deserialize(deserializer).map(|opt| opt.and_then(Decimal::from_f64))
    }
}

/// Proxy-specific context for order events.
///
/// Contains additional metadata that the proxy adds to order events,
/// such as SL/TP tracking info, position quantities, and cancellation reasons.
/// This data is NOT part of the provider's order model.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OrderContext {
    /// Current position quantity after this event (for position close detection).
    /// Only populated when the provider's WebSocket includes position info.
    #[serde(skip_serializing_if = "Option::is_none", with = "decimal_float_option")]
    pub position_qty: Option<Decimal>,

    /// Quantity filled in THIS specific execution event (the delta, not cumulative).
    /// Used for position synthesis when provider doesn't supply `position_qty`.
    /// For Binance: this is `last_executed_qty` from execution reports.
    #[serde(skip_serializing_if = "Option::is_none", with = "decimal_float_option")]
    pub last_fill_qty: Option<Decimal>,

    /// SL/TP order type: `stop_loss` or `take_profit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sl_tp_type: Option<String>,

    /// The primary order ID that triggered this SL/TP.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_order_id: Option<String>,

    /// Placeholder ID assigned before the order was placed with the provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placeholder_id: Option<String>,

    /// Whether the order was actually placed with the provider (false if cancelled before placement).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub was_placed: Option<bool>,

    /// Reason for cancellation (e.g., `primary_order_cancelled`, `sibling_filled`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancel_reason: Option<String>,

    /// ID of the order that caused this cancellation (for sibling cancellations).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancelled_by: Option<String>,
}

impl OrderContext {
    /// Create an empty context.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add position quantity to context.
    #[must_use]
    pub fn with_position_qty(mut self, qty: Decimal) -> Self {
        self.position_qty = Some(qty);
        self
    }

    /// Add last fill quantity (delta for this execution event, not cumulative).
    #[must_use]
    pub fn with_last_fill_qty(mut self, qty: Decimal) -> Self {
        self.last_fill_qty = Some(qty);
        self
    }

    /// Add SL/TP context.
    #[must_use]
    pub fn with_sl_tp(
        mut self,
        sl_tp_type: impl Into<String>,
        primary_order_id: impl Into<String>,
        placeholder_id: impl Into<String>,
    ) -> Self {
        self.sl_tp_type = Some(sl_tp_type.into());
        self.primary_order_id = Some(primary_order_id.into());
        self.placeholder_id = Some(placeholder_id.into());
        self
    }

    /// Mark as placed with provider.
    #[must_use]
    pub const fn mark_placed(mut self) -> Self {
        self.was_placed = Some(true);
        self
    }

    /// Mark as not placed with provider.
    #[must_use]
    pub const fn mark_not_placed(mut self) -> Self {
        self.was_placed = Some(false);
        self
    }

    /// Add cancellation reason.
    #[must_use]
    pub fn with_cancel_reason(
        mut self,
        reason: impl Into<String>,
        cancelled_by: Option<String>,
    ) -> Self {
        self.cancel_reason = Some(reason.into());
        self.cancelled_by = cancelled_by;
        self
    }
}

/// Error notification event
///
/// Represents an error that occurred during trading operations.
#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct ErrorEvent {
    /// Unique correlation ID for tracking this event
    pub correlation_id: String,

    /// Error type/category
    #[validate(length(min = 1))]
    pub error_type: String,

    /// Human-readable error message
    #[validate(length(min = 1))]
    pub message: String,

    /// Event timestamp (Unix milliseconds)
    pub timestamp: u64,

    /// Severity level for risk assessment
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severity: Option<FailureSeverity>,

    /// Suggested action for the strategy
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_action: Option<SuggestedAction>,

    /// Additional context (type-specific details)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<serde_json::Value>,
}

impl ErrorEvent {
    /// Create a new error event with auto-generated correlation ID.
    pub fn new(error_type: String, message: String, timestamp: u64) -> Self {
        Self {
            correlation_id: Uuid::new_v4().to_string(),
            error_type,
            message,
            timestamp,
            severity: None,
            suggested_action: None,
            context: None,
        }
    }

    /// Add severity and suggested action to an error event.
    #[must_use]
    pub const fn with_severity(
        mut self,
        severity: FailureSeverity,
        suggested_action: SuggestedAction,
    ) -> Self {
        self.severity = Some(severity);
        self.suggested_action = Some(suggested_action);
        self
    }

    /// Add context details to an error event.
    #[must_use]
    pub fn with_context(mut self, context: serde_json::Value) -> Self {
        self.context = Some(context);
        self
    }

    /// Check if this is a critical error.
    #[must_use]
    pub fn is_critical(&self) -> bool {
        self.severity == Some(FailureSeverity::Critical)
    }

    /// Check if this error suggests closing position immediately.
    #[must_use]
    pub fn should_close_immediately(&self) -> bool {
        self.suggested_action == Some(SuggestedAction::ClosePositionImmediately)
    }

    /// Check if retry is suggested for this error.
    #[must_use]
    pub fn should_retry(&self) -> bool {
        self.suggested_action == Some(SuggestedAction::RetryPossible)
    }
}

/// Per-instrument subscription with wildcard support
#[derive(Debug, Clone, Serialize, Deserialize, Validate, PartialEq, Eq)]
pub struct InstrumentSubscription {
    /// Platform for this subscription
    pub platform: TradingPlatform,

    /// Instrument symbol or pattern
    #[validate(length(
        min = 1,
        max = 50,
        message = "instrument pattern must be 1-50 characters"
    ))]
    pub instrument: String,

    /// Event patterns
    #[validate(length(min = 1, max = 100, message = "events must contain 1-100 patterns"))]
    pub events: Vec<String>,
}

impl InstrumentSubscription {
    /// Create a new instrument subscription.
    #[must_use]
    pub const fn new(platform: TradingPlatform, instrument: String, events: Vec<String>) -> Self {
        Self {
            platform,
            instrument,
            events,
        }
    }
}

// =============================================================================
// Internal event types (not sent to strategies)
// =============================================================================

/// Internal trading event with additional context.
///
/// This wrapper carries `WsMessage` events along with provider-specific context
/// that's needed for internal processing (e.g., position synthesis) but is NOT
/// sent to strategies. The internal channel uses this type; the strategy-facing
/// channel uses plain `WsMessage`.
///
/// # Position Event Synthesis
///
/// When Alpaca sends a fill event with `position_qty`, we capture it in `context`
/// and pass it to `EventRouter`. The router uses `StateManager` to determine if
/// this is a `PositionOpened`/Modified/Closed event and broadcasts accordingly.
#[derive(Debug, Clone)]
pub struct InternalTradingEvent {
    /// The WebSocket message to broadcast.
    pub message: WsMessage,
    /// Optional context with `position_qty` and other metadata.
    pub context: Option<OrderContext>,
    /// The platform this event came from (needed to route to correct `EventRouter`).
    pub platform: TradingPlatform,
}

impl InternalTradingEvent {
    /// Create an internal event from a `WsMessage` without context.
    #[must_use]
    pub const fn new(message: WsMessage, platform: TradingPlatform) -> Self {
        Self {
            message,
            context: None,
            platform,
        }
    }

    /// Create an internal event from a `WsMessage` with context.
    #[must_use]
    pub const fn with_context(
        message: WsMessage,
        context: OrderContext,
        platform: TradingPlatform,
    ) -> Self {
        Self {
            message,
            context: Some(context),
            platform,
        }
    }
}

/// Event acknowledgment message (sent from strategy to provider)
#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct EventAckMessage {
    /// Unique correlation ID for tracking this acknowledgment
    pub correlation_id: String,

    /// List of event correlation IDs that were processed
    #[validate(length(min = 1))]
    pub events_processed: Vec<String>,

    /// Timestamp when events were processed (Unix milliseconds)
    pub timestamp: u64,
}

impl EventAckMessage {
    /// Create a new event acknowledgment message with auto-generated correlation ID.
    pub fn new(events_processed: Vec<String>, timestamp: u64) -> Self {
        Self {
            correlation_id: Uuid::new_v4().to_string(),
            events_processed,
            timestamp,
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // === Event tests ===

    #[test]
    fn test_event_parse_candle() {
        let result: Result<Event, _> = "candle_1m".parse();
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Event::Candle(Timeframe::OneMinute));
    }

    #[test]
    fn test_event_parse_quote() {
        let result: Result<Event, _> = "quote".parse();
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Event::Quote);
    }

    #[test]
    fn test_event_parse_order_update() {
        let result: Result<Event, _> = "order_update".parse();
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Event::OrderUpdate);
    }

    #[test]
    fn test_event_parse_position_update() {
        let result: Result<Event, _> = "position_update".parse();
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Event::PositionUpdate);
    }

    #[test]
    fn test_event_display_candle() {
        let event = Event::Candle(Timeframe::OneMinute);
        assert_eq!(event.to_string(), "candle_1m");
    }

    #[test]
    fn test_event_display_quote() {
        assert_eq!(Event::Quote.to_string(), "quote");
    }

    #[test]
    fn test_event_display_order_update() {
        let event = Event::OrderUpdate;
        assert_eq!(event.to_string(), "order_update");
    }

    #[test]
    fn test_event_parse_invalid() {
        let result: Result<Event, _> = "invalid_event".parse();
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            EventParseError::InvalidEvent(_)
        ));
    }

    #[test]
    fn test_event_roundtrip() {
        let original = Event::Candle(Timeframe::FiveMinutes);
        let string_repr = original.to_string();
        let parsed: Event = string_repr.parse().unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn test_all_non_candle_events() {
        let events = vec![
            ("quote", Event::Quote),
            ("option_greeks", Event::OptionGreeks),
            ("order_update", Event::OrderUpdate),
            ("position_update", Event::PositionUpdate),
            ("account_update", Event::AccountUpdate),
            ("trade_update", Event::TradeUpdate),
        ];

        for (string_repr, expected_event) in events {
            let parsed: Event = string_repr.parse().unwrap();
            assert_eq!(parsed, expected_event);
            assert_eq!(expected_event.to_string(), string_repr);
        }
    }

    #[test]
    fn test_all_candle_timeframes_roundtrip() {
        let timeframes = [
            Timeframe::OneMinute,
            Timeframe::TwoMinutes,
            Timeframe::FiveMinutes,
            Timeframe::TenMinutes,
            Timeframe::FifteenMinutes,
            Timeframe::ThirtyMinutes,
            Timeframe::OneHour,
            Timeframe::TwoHours,
            Timeframe::FourHours,
            Timeframe::TwelveHours,
            Timeframe::OneDay,
            Timeframe::OneWeek,
        ];

        for tf in timeframes {
            let event = Event::Candle(tf);
            let string = event.to_string();
            let parsed: Event = string.parse().unwrap();
            assert_eq!(event, parsed);
        }
    }

    #[test]
    fn test_parse_case_sensitive() {
        // Ensure event names are case-sensitive
        let result: Result<Event, _> = "ORDER_UPDATE".parse();
        assert!(result.is_err());

        let result: Result<Event, _> = "Candle_1m".parse();
        assert!(result.is_err());
    }

    #[test]
    fn test_empty_string() {
        let result: Result<Event, _> = "".parse();
        assert!(result.is_err());
    }

    #[test]
    fn test_whitespace() {
        let result: Result<Event, _> = " order_update".parse();
        assert!(result.is_err());

        let result: Result<Event, _> = "order_update ".parse();
        assert!(result.is_err());
    }

    // === Broker connection event tests ===

    #[test]
    fn test_broker_disconnected_serialization() {
        let msg = WsMessage::broker_disconnected(TradingPlatform::AlpacaPaper);
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""event":"BROKER_DISCONNECTED""#));
        assert!(json.contains(r#""broker":"alpaca-paper""#));
        // No gap_duration_ms for disconnect
        assert!(!json.contains("gap_duration_ms"));
    }

    #[test]
    fn test_broker_reconnected_includes_gap() {
        let gap = std::time::Duration::from_millis(12345);
        let msg = WsMessage::broker_reconnected(TradingPlatform::AlpacaPaper, gap);
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""event":"BROKER_RECONNECTED""#));
        assert!(json.contains(r#""broker":"alpaca-paper""#));
        assert!(json.contains(r#""gap_duration_ms":12345"#));
    }

    #[test]
    fn test_broker_connection_failed_serialization() {
        let msg = WsMessage::broker_connection_failed(TradingPlatform::AlpacaPaper);
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""event":"BROKER_CONNECTION_FAILED""#));
        assert!(json.contains(r#""broker":"alpaca-paper""#));
    }

    #[test]
    fn test_existing_connection_events_backward_compat() {
        // Existing Connection events should not include broker or gap fields
        let msg = WsMessage::Connection {
            event: ConnectionEventType::Connected,
            error: None,
            broker: None,
            gap_duration_ms: None,
            timestamp: Utc::now(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""event":"CONNECTED""#));
        assert!(!json.contains("broker"));
        assert!(!json.contains("gap_duration_ms"));

        let msg = WsMessage::Connection {
            event: ConnectionEventType::Disconnecting,
            error: None,
            broker: None,
            gap_duration_ms: None,
            timestamp: Utc::now(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""event":"DISCONNECTING""#));
        assert!(!json.contains("broker"));
    }

    // === DataStaleness tests ===

    #[test]
    fn test_data_stale_serialization() {
        let msg = WsMessage::DataStaleness {
            event: DataStalenessEventType::Stale,
            symbols: vec!["AAPL".to_string(), "TSLA".to_string()],
            stale_since: Some(
                DateTime::parse_from_rfc3339("2026-03-31T14:30:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            ),
            broker: Some("alpaca-paper".to_string()),
            timestamp: DateTime::parse_from_rfc3339("2026-03-31T14:30:00Z")
                .unwrap()
                .with_timezone(&Utc),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"data_staleness""#));
        assert!(json.contains(r#""event":"STALE""#));
        assert!(json.contains(r#""symbols":["AAPL","TSLA"]"#));
        assert!(json.contains(r#""stale_since":"#));
        assert!(json.contains(r#""broker":"alpaca-paper""#));
    }

    #[test]
    fn test_data_fresh_serialization() {
        let stale_since = DateTime::parse_from_rfc3339("2026-03-31T14:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let msg = WsMessage::DataStaleness {
            event: DataStalenessEventType::Fresh,
            symbols: vec!["AAPL".to_string()],
            stale_since: Some(stale_since),
            broker: Some("alpaca-paper".to_string()),
            timestamp: DateTime::parse_from_rfc3339("2026-03-31T14:30:05Z")
                .unwrap()
                .with_timezone(&Utc),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""event":"FRESH""#));
        assert!(json.contains(r#""symbols":["AAPL"]"#));
        assert!(json.contains(r#""stale_since":"#));
    }

    #[test]
    fn test_data_staleness_roundtrip() {
        let msg = WsMessage::DataStaleness {
            event: DataStalenessEventType::Stale,
            symbols: vec!["EURUSD".to_string()],
            stale_since: Some(Utc::now()),
            broker: Some("oanda-live".to_string()),
            timestamp: Utc::now(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: WsMessage = serde_json::from_str(&json).unwrap();
        if let WsMessage::DataStaleness { event, symbols, .. } = deserialized {
            assert_eq!(event, DataStalenessEventType::Stale);
            assert_eq!(symbols, vec!["EURUSD"]);
        } else {
            panic!("Expected DataStaleness variant");
        }
    }

    #[test]
    fn test_data_stale_omits_none_fields() {
        let msg = WsMessage::DataStaleness {
            event: DataStalenessEventType::Stale,
            symbols: vec!["AAPL".to_string()],
            stale_since: None,
            broker: None,
            timestamp: Utc::now(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(!json.contains("stale_since"));
        assert!(!json.contains("broker"));
    }
}
