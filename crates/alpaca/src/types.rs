//! Alpaca-specific request and response types.
//!
//! These types map to the Alpaca Trading API and Market Data API responses.
//! They are used internally by the `AlpacaAdapter` for serialization/deserialization.

use serde::{Deserialize, Serialize};

/// Alpaca-specific account structure
#[derive(Debug, Deserialize)]
pub struct AlpacaAccount {
    /// Account ID (required for API deserialization, may not be used directly)
    #[allow(dead_code)]
    pub id: String,
    /// Currency code (e.g., "USD")
    pub currency: String,
    /// Cash balance as string
    pub cash: String,
    /// Total portfolio value as string
    pub portfolio_value: String,
    /// Buying power available as string
    pub buying_power: String,
}

/// Alpaca-specific order structure
#[derive(Debug, Serialize, Deserialize)]
pub struct AlpacaOrder {
    /// Order ID
    pub id: String,
    /// Client-provided order ID (optional)
    pub client_order_id: Option<String>,
    /// Trading symbol
    pub symbol: String,
    /// Quantity as string
    pub qty: String,
    /// Side: "buy" or "sell"
    pub side: String,
    /// Order type: "market", "limit", etc.
    #[serde(rename = "type")]
    pub order_type: String,
    /// Time in force: "day", "gtc", etc.
    pub time_in_force: String,
    /// Order status: "new", "filled", "canceled", etc.
    pub status: String,
    /// Quantity filled as string
    pub filled_qty: String,
    /// Average fill price as string (optional)
    pub filled_avg_price: Option<String>,
    /// Limit price as string (optional, for limit orders)
    pub limit_price: Option<String>,
    /// Stop price as string (optional, for stop orders)
    pub stop_price: Option<String>,
    /// Order creation timestamp
    pub created_at: String,
    /// Order last update timestamp
    pub updated_at: String,
    /// Leg orders for bracket/OTO orders (stop-loss, take-profit)
    #[serde(default)]
    pub legs: Option<Vec<AlpacaOrder>>,
}

/// Alpaca position structure
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct AlpacaPosition {
    /// Trading symbol
    pub symbol: String,
    /// Position quantity as string
    pub qty: String,
    /// Average entry price as string
    pub avg_entry_price: String,
    /// Current market price as string
    pub current_price: String,
    /// Total market value as string
    pub market_value: String,
    /// Unrealized profit/loss as string
    pub unrealized_pl: String,
    /// Unrealized profit/loss percentage as string
    pub unrealized_plpc: String,
}

/// Alpaca trade activity (fill)
#[derive(Debug, Deserialize)]
pub struct AlpacaActivity {
    /// Activity ID
    pub id: String,
    /// Activity type (e.g., "FILL")
    #[allow(dead_code)] // Required for deserialization from Alpaca API
    pub activity_type: String,
    /// Associated order ID
    pub order_id: String,
    /// Trading symbol
    pub symbol: String,
    /// Trade side: "buy" or "sell"
    pub side: String,
    /// Trade quantity as string
    pub qty: String,
    /// Execution price as string
    pub price: String,
    /// Transaction timestamp
    pub transaction_time: String,
}

/// Alpaca order request
#[derive(Debug, Clone, Serialize)]
pub struct AlpacaOrderRequest {
    /// Trading symbol
    pub symbol: String,
    /// Quantity as string
    pub qty: String,
    /// Side: "buy" or "sell"
    pub side: String,
    /// Order type: "market", "limit", etc.
    #[serde(rename = "type")]
    pub order_type: String,
    /// Time in force: "day", "gtc", etc.
    pub time_in_force: String,
    /// Limit price (required for limit orders)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_price: Option<String>,
    /// Stop price (required for stop orders)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_price: Option<String>,
    /// Order class: "simple", "bracket", "oco", "oto"
    /// Required for bracket orders with `stop_loss/take_profit`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_class: Option<String>,
    /// Stop loss configuration for bracket orders
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_loss: Option<AlpacaStopLoss>,
    /// Take profit configuration for bracket orders
    #[serde(skip_serializing_if = "Option::is_none")]
    pub take_profit: Option<AlpacaTakeProfit>,
    /// Client-provided order ID for idempotent retries
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_order_id: Option<String>,
}

/// Alpaca stop loss configuration for bracket orders
#[derive(Debug, Clone, Serialize)]
pub struct AlpacaStopLoss {
    /// Stop price at which to trigger the stop loss
    pub stop_price: String,
}

/// Alpaca take profit configuration for bracket orders
#[derive(Debug, Clone, Serialize)]
pub struct AlpacaTakeProfit {
    /// Limit price for take profit order
    pub limit_price: String,
}

/// Alpaca order modification request (PATCH /`v2/orders/{order_id`})
///
/// All fields are optional - only include fields that should be modified.
#[derive(Debug, Serialize, Default)]
pub struct AlpacaModifyOrderRequest {
    /// New quantity (must be >= filled quantity)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qty: Option<String>,
    /// New time in force
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_in_force: Option<String>,
    /// New limit price
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_price: Option<String>,
    /// New stop price
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_price: Option<String>,
    /// Optional new client order ID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_order_id: Option<String>,
}

/// Alpaca quote response structure (stocks - v2 API)
/// Format: {"quote": {...}, "symbol": "AAPL"}
#[derive(Debug, Deserialize)]
pub struct AlpacaQuoteResponse {
    /// Quote data
    pub quote: AlpacaQuote,
}

/// Alpaca crypto quotes response structure (crypto - v1beta3 API)
/// Format: {"quotes": {"BTC/USD": {...}}}
#[derive(Debug, Deserialize)]
pub struct AlpacaCryptoQuotesResponse {
    /// Map of symbol to quote data
    pub quotes: std::collections::HashMap<String, AlpacaQuote>,
}

/// Alpaca quote data
#[derive(Debug, Deserialize)]
pub struct AlpacaQuote {
    /// Quote timestamp
    #[serde(rename = "t")]
    pub timestamp: String,
    /// Bid price
    #[serde(rename = "bp")]
    pub bid_price: f64,
    /// Ask price
    #[serde(rename = "ap")]
    pub ask_price: f64,
    /// Bid size
    #[serde(rename = "bs")]
    pub bid_size: f64,
    /// Ask size
    #[serde(rename = "as")]
    pub ask_size: f64,
}

/// Alpaca bars response structure (for stocks - v2 API)
/// Note: bars can be null when market is closed or no data available
#[derive(Debug, Deserialize)]
pub struct AlpacaBarsResponse {
    /// Array of bars (defaults to empty vec if null or missing)
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub bars: Vec<AlpacaBar>,
}

fn deserialize_null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + serde::Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// Alpaca crypto bars response structure (for crypto - v1beta3 API)
/// Crypto API returns bars grouped by symbol: {"bars": {"BTC/USD": [...]}}
#[derive(Debug, Deserialize)]
pub struct AlpacaCryptoBarsResponse {
    /// Map of symbol to bars
    pub bars: std::collections::HashMap<String, Vec<AlpacaBar>>,
}

/// Alpaca bar data (OHLCV candlestick)
#[derive(Debug, Deserialize)]
pub struct AlpacaBar {
    /// Bar timestamp
    #[serde(rename = "t")]
    pub timestamp: String,
    /// Open price
    #[serde(rename = "o")]
    pub open: f64,
    /// High price
    #[serde(rename = "h")]
    pub high: f64,
    /// Low price
    #[serde(rename = "l")]
    pub low: f64,
    /// Close price
    #[serde(rename = "c")]
    pub close: f64,
    /// Volume
    #[serde(rename = "v")]
    pub volume: f64,
}
