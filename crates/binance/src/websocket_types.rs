//! Serde types for Binance WebSocket messages.
//!
//! Binance Spot/Margin and Futures use structurally different event schemas,
//! so they are modelled as separate types rather than a single shared struct.

use serde::Deserialize;

// ============================================================================
// Combined Stream Envelope
// ============================================================================

/// Wrapper for Binance combined-stream messages.
///
/// Combined streams (`/stream?streams=...`) wrap each payload in:
/// ```json
/// {"stream": "btcusdt@kline_1m", "data": { ... }}
/// ```
#[derive(Debug, Deserialize)]
pub struct CombinedStreamMessage {
    /// Stream name (e.g., `btcusdt@kline_1m`, `btcusdt@bookTicker`).
    pub stream: String,
    /// Raw payload — deserialized later based on stream suffix.
    pub data: serde_json::Value,
}

// ============================================================================
// Market Data — Book Ticker
// ============================================================================

/// Best bid/ask update from `@bookTicker` stream.
#[derive(Debug, Deserialize)]
pub struct WsBookTickerEvent {
    /// Update ID.
    #[serde(rename = "u")]
    #[allow(dead_code)]
    pub update_id: u64,
    /// Symbol (uppercase, e.g. `BTCUSDT`).
    #[serde(rename = "s")]
    pub symbol: String,
    /// Best bid price.
    #[serde(rename = "b")]
    pub bid_price: String,
    /// Best bid quantity.
    #[serde(rename = "B")]
    pub bid_qty: String,
    /// Best ask price.
    #[serde(rename = "a")]
    pub ask_price: String,
    /// Best ask quantity.
    #[serde(rename = "A")]
    pub ask_qty: String,
}

// ============================================================================
// Market Data — Kline
// ============================================================================

/// Kline/candlestick event from `@kline_<interval>` stream.
#[derive(Debug, Deserialize)]
pub struct WsKlineEvent {
    /// Event type (`kline`).
    #[serde(rename = "e")]
    #[allow(dead_code)]
    pub event_type: String,
    /// Event time (ms).
    #[serde(rename = "E")]
    #[allow(dead_code)]
    pub event_time: u64,
    /// Symbol.
    #[serde(rename = "s")]
    pub symbol: String,
    /// Kline data.
    #[serde(rename = "k")]
    pub kline: WsKlineData,
}

/// Inner kline data object.
#[derive(Debug, Deserialize)]
pub struct WsKlineData {
    /// Kline start time (ms).
    #[serde(rename = "t")]
    pub open_time: i64,
    /// Kline close time (ms).
    #[serde(rename = "T")]
    #[allow(dead_code)]
    pub close_time: i64,
    /// Interval (e.g. `1m`, `1h`).
    #[serde(rename = "i")]
    pub interval: String,
    /// Open price.
    #[serde(rename = "o")]
    pub open: String,
    /// High price.
    #[serde(rename = "h")]
    pub high: String,
    /// Low price.
    #[serde(rename = "l")]
    pub low: String,
    /// Close price.
    #[serde(rename = "c")]
    pub close: String,
    /// Volume.
    #[serde(rename = "v")]
    pub volume: String,
    /// Is this kline closed?
    #[serde(rename = "x")]
    #[allow(dead_code)]
    pub is_closed: bool,
}

// ============================================================================
// Spot / Margin — User Data Stream
// ============================================================================

/// Spot/Margin order execution report (`executionReport`).
#[derive(Debug, Deserialize)]
pub struct SpotExecutionReport {
    /// Event type (`executionReport`).
    #[serde(rename = "e")]
    #[allow(dead_code)]
    pub event_type: String,
    /// Event time (ms).
    #[serde(rename = "E")]
    #[allow(dead_code)]
    pub event_time: u64,
    /// Symbol.
    #[serde(rename = "s")]
    pub symbol: String,
    /// Client order ID.
    #[serde(rename = "c")]
    pub client_order_id: String,
    /// Side (`BUY` / `SELL`).
    #[serde(rename = "S")]
    pub side: String,
    /// Order type (`LIMIT`, `MARKET`, etc.).
    #[serde(rename = "o")]
    pub order_type: String,
    /// Time in force (`GTC`, `IOC`, `FOK`).
    #[serde(rename = "f")]
    pub time_in_force: String,
    /// Original quantity.
    #[serde(rename = "q")]
    pub quantity: String,
    /// Limit price.
    #[serde(rename = "p")]
    pub price: String,
    /// Stop price.
    #[serde(rename = "P")]
    pub stop_price: String,
    /// Execution type (`NEW`, `CANCELED`, `TRADE`, `EXPIRED`, `REJECTED`).
    #[serde(rename = "x")]
    pub execution_type: String,
    /// Order status (`NEW`, `PARTIALLY_FILLED`, `FILLED`, `CANCELED`, `REJECTED`, `EXPIRED`).
    #[serde(rename = "X")]
    pub order_status: String,
    /// Reject reason (if any).
    #[serde(rename = "r")]
    pub reject_reason: String,
    /// Order ID.
    #[serde(rename = "i")]
    pub order_id: u64,
    /// Last filled quantity (delta, not cumulative).
    #[serde(rename = "l")]
    pub last_executed_qty: String,
    /// Cumulative filled quantity.
    #[serde(rename = "z")]
    pub cumulative_filled_qty: String,
    /// Last filled price.
    #[serde(rename = "L")]
    pub last_executed_price: String,
    /// Commission amount.
    #[serde(rename = "n")]
    pub commission: String,
    /// Commission asset.
    #[serde(rename = "N")]
    pub commission_asset: Option<String>,
    /// Transaction time (ms).
    #[serde(rename = "T")]
    pub transaction_time: i64,
    /// Original client order ID (for cancels, this is the cancelled order's ID).
    #[serde(rename = "C")]
    pub original_client_order_id: String,
}

/// Spot/Margin account position update (`outboundAccountPosition`).
#[derive(Debug, Deserialize)]
pub struct SpotAccountPositionUpdate {
    /// Event type.
    #[serde(rename = "e")]
    #[allow(dead_code)]
    pub event_type: String,
    /// Event time (ms).
    #[serde(rename = "E")]
    #[allow(dead_code)]
    pub event_time: u64,
    /// Balances that changed.
    #[serde(rename = "B")]
    pub balances: Vec<SpotBalanceUpdate>,
}

/// Individual balance update within `outboundAccountPosition`.
#[derive(Debug, Deserialize)]
pub struct SpotBalanceUpdate {
    /// Asset name.
    #[serde(rename = "a")]
    pub asset: String,
    /// Free balance.
    #[serde(rename = "f")]
    pub free: String,
    /// Locked balance.
    #[serde(rename = "l")]
    pub locked: String,
}

// ============================================================================
// Futures / Coin-Futures — User Data Stream
// ============================================================================

/// Futures order update (`ORDER_TRADE_UPDATE`).
#[derive(Debug, Deserialize)]
pub struct FuturesOrderTradeUpdate {
    /// Event type (`ORDER_TRADE_UPDATE`).
    #[serde(rename = "e")]
    #[allow(dead_code)]
    pub event_type: String,
    /// Event time (ms).
    #[serde(rename = "E")]
    #[allow(dead_code)]
    pub event_time: u64,
    /// Transaction time (ms).
    #[serde(rename = "T")]
    #[allow(dead_code)]
    pub transaction_time: u64,
    /// Order data.
    #[serde(rename = "o")]
    pub order: FuturesOrderData,
}

/// Inner order data within `ORDER_TRADE_UPDATE`.
#[derive(Debug, Deserialize)]
pub struct FuturesOrderData {
    /// Symbol.
    #[serde(rename = "s")]
    pub symbol: String,
    /// Client order ID.
    #[serde(rename = "c")]
    pub client_order_id: String,
    /// Side (`BUY` / `SELL`).
    #[serde(rename = "S")]
    pub side: String,
    /// Order type.
    #[serde(rename = "o")]
    pub order_type: String,
    /// Time in force.
    #[serde(rename = "f")]
    pub time_in_force: String,
    /// Original quantity.
    #[serde(rename = "q")]
    pub quantity: String,
    /// Limit price.
    #[serde(rename = "p")]
    pub price: String,
    /// Stop price.
    #[serde(rename = "sp")]
    pub stop_price: String,
    /// Average price.
    #[serde(rename = "ap")]
    pub average_price: String,
    /// Execution type.
    #[serde(rename = "x")]
    pub execution_type: String,
    /// Order status.
    #[serde(rename = "X")]
    pub order_status: String,
    /// Order ID.
    #[serde(rename = "i")]
    pub order_id: u64,
    /// Last filled quantity (delta).
    #[serde(rename = "l")]
    pub last_filled_qty: String,
    /// Cumulative filled quantity.
    #[serde(rename = "z")]
    pub cumulative_filled_qty: String,
    /// Last filled price.
    #[serde(rename = "L")]
    pub last_filled_price: String,
    /// Commission.
    #[serde(rename = "n")]
    pub commission: String,
    /// Commission asset.
    #[serde(rename = "N")]
    pub commission_asset: Option<String>,
    /// Realized profit.
    #[serde(rename = "rp")]
    pub realized_profit: String,
    /// Position side (`BOTH`, `LONG`, `SHORT`).
    #[serde(rename = "ps")]
    pub position_side: String,
    /// Reduce only.
    #[serde(rename = "R")]
    pub reduce_only: bool,
    /// Callback rate for trailing stop (percentage).
    #[serde(rename = "cr")]
    pub callback_rate: Option<String>,
    /// Activation price for trailing stop.
    #[serde(rename = "AP")]
    pub activation_price: Option<String>,
}

/// Futures account update (`ACCOUNT_UPDATE`).
#[derive(Debug, Deserialize)]
pub struct FuturesAccountUpdate {
    /// Event type (`ACCOUNT_UPDATE`).
    #[serde(rename = "e")]
    #[allow(dead_code)]
    pub event_type: String,
    /// Event time (ms).
    #[serde(rename = "E")]
    #[allow(dead_code)]
    pub event_time: u64,
    /// Account data.
    #[serde(rename = "a")]
    pub account: FuturesAccountData,
}

/// Inner account data within `ACCOUNT_UPDATE`.
#[derive(Debug, Deserialize)]
pub struct FuturesAccountData {
    /// Update reason (`ORDER`, `FUNDING_FEE`, `DEPOSIT`, etc.).
    #[serde(rename = "m")]
    pub reason: String,
    /// Balance updates.
    #[serde(rename = "B")]
    pub balances: Vec<FuturesBalanceUpdate>,
    /// Position updates.
    #[serde(rename = "P")]
    pub positions: Vec<FuturesPositionUpdate>,
}

/// Individual futures balance update.
#[derive(Debug, Deserialize)]
pub struct FuturesBalanceUpdate {
    /// Asset.
    #[serde(rename = "a")]
    pub asset: String,
    /// Wallet balance.
    #[serde(rename = "wb")]
    pub wallet_balance: String,
    /// Cross wallet balance.
    #[serde(rename = "cw")]
    pub cross_wallet_balance: String,
}

/// Individual futures position update.
#[derive(Debug, Deserialize)]
pub struct FuturesPositionUpdate {
    /// Symbol.
    #[serde(rename = "s")]
    pub symbol: String,
    /// Position amount.
    #[serde(rename = "pa")]
    pub position_amount: String,
    /// Entry price.
    #[serde(rename = "ep")]
    pub entry_price: String,
    /// Unrealized `PnL`.
    #[serde(rename = "up")]
    pub unrealized_pnl: String,
    /// Margin type (`cross` / `isolated`).
    #[serde(rename = "mt")]
    pub margin_type: String,
    /// Position side.
    #[serde(rename = "ps")]
    pub position_side: String,
}

// ============================================================================
// Generic User Data Stream Envelope
// ============================================================================

/// Minimal envelope to peek at the event type before full deserialization.
#[derive(Debug, Deserialize)]
pub struct UserDataEventEnvelope {
    /// Event type string (`executionReport`, `outboundAccountPosition`,
    /// `ORDER_TRADE_UPDATE`, `ACCOUNT_UPDATE`, etc.).
    #[serde(rename = "e")]
    pub event_type: String,
}
