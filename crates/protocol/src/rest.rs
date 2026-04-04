//! REST API wire types for communication with the Tektii Engine.
//!
//! These types exactly match the engine's REST API. They are purpose-built for
//! simulation and simpler than the gateway's normalised models.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

// =============================================================================
// Enums
// =============================================================================

/// Order side (buy or sell).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

/// Order type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderType {
    Market,
    Limit,
    Stop,
    StopLimit,
}

/// Order status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    Open,
    /// Order has been partially filled; some quantity remains.
    PartiallyFilled,
    Filled,
    Cancelled,
    Rejected,
}

/// Position side (long or short).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PositionSide {
    Long,
    Short,
}

/// Reason for order rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectionReason {
    InsufficientMargin,
    InvalidSymbol,
    InvalidQuantity,
    PriceOutOfBounds,
    PositionNotFound,
    ReduceOnlyViolated,
    PositionSizeLimitExceeded,
}

/// Order event type for WebSocket messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderEventType {
    Created,
    /// Order has been partially filled; some quantity remains.
    PartialFill,
    Filled,
    Cancelled,
    Rejected,
}

/// Account event type for WebSocket messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountEventType {
    /// Account balance was updated (deposit, withdrawal, trade P&L).
    BalanceUpdated,
    /// Margin usage warning (approaching limit).
    MarginWarning,
    /// Margin call - positions were liquidated due to insufficient margin.
    MarginCall,
}

// =============================================================================
// Request Types
// =============================================================================

/// Request body for submitting a new order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitOrderRequest {
    /// Instrument symbol (e.g., "AAPL", "BTC-USD").
    pub symbol: String,

    /// Order side (buy or sell).
    pub side: Side,

    /// Order type (market, limit, or stop).
    pub order_type: OrderType,

    /// Order quantity.
    pub quantity: Decimal,

    /// Limit price (required for limit orders).
    #[serde(default)]
    pub limit_price: Option<Decimal>,

    /// Stop/trigger price (required for stop orders).
    #[serde(default)]
    pub stop_price: Option<Decimal>,

    /// Client-provided order ID for correlation.
    #[serde(default)]
    pub client_order_id: Option<String>,

    /// Target position ID for close/reduce orders (hedging mode only).
    /// In netting mode, this field is ignored.
    #[serde(default)]
    pub position_id: Option<String>,

    /// If true, order can only reduce existing position, never open new.
    /// Prevents accidental position flip.
    #[serde(default)]
    pub reduce_only: bool,
}

impl SubmitOrderRequest {
    /// Create a market order request.
    pub fn market(symbol: impl Into<String>, side: Side, quantity: Decimal) -> Self {
        Self {
            symbol: symbol.into(),
            side,
            order_type: OrderType::Market,
            quantity,
            limit_price: None,
            stop_price: None,
            client_order_id: None,
            position_id: None,
            reduce_only: false,
        }
    }

    /// Create a limit order request.
    pub fn limit(symbol: impl Into<String>, side: Side, quantity: Decimal, price: Decimal) -> Self {
        Self {
            symbol: symbol.into(),
            side,
            order_type: OrderType::Limit,
            quantity,
            limit_price: Some(price),
            stop_price: None,
            client_order_id: None,
            position_id: None,
            reduce_only: false,
        }
    }

    /// Create a stop order request.
    pub fn stop(
        symbol: impl Into<String>,
        side: Side,
        quantity: Decimal,
        stop_price: Decimal,
    ) -> Self {
        Self {
            symbol: symbol.into(),
            side,
            order_type: OrderType::Stop,
            quantity,
            limit_price: None,
            stop_price: Some(stop_price),
            client_order_id: None,
            position_id: None,
            reduce_only: false,
        }
    }

    /// Create a stop-limit order request.
    pub fn stop_limit(
        symbol: impl Into<String>,
        side: Side,
        quantity: Decimal,
        stop_price: Decimal,
        limit_price: Decimal,
    ) -> Self {
        Self {
            symbol: symbol.into(),
            side,
            order_type: OrderType::StopLimit,
            quantity,
            limit_price: Some(limit_price),
            stop_price: Some(stop_price),
            client_order_id: None,
            position_id: None,
            reduce_only: false,
        }
    }
}

/// Query parameters for listing orders.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OrderQueryParams {
    /// Filter by symbol.
    pub symbol: Option<String>,

    /// Filter by status.
    pub status: Option<OrderStatus>,

    /// Maximum number of results (default: 100).
    pub limit: Option<u32>,
}

/// Query parameters for listing trades.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TradeQueryParams {
    /// Filter by symbol.
    pub symbol: Option<String>,
}

/// Query parameters for cancelling all orders.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CancelAllQueryParams {
    /// Filter by symbol (optional).
    pub symbol: Option<String>,
}

// =============================================================================
// Response Types
// =============================================================================

/// Minimal order handle returned from order creation/cancellation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderHandle {
    /// Order ID.
    pub id: String,

    /// Client-provided order ID (if any).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_order_id: Option<String>,

    /// Current order status.
    pub status: OrderStatus,
}

/// Full order details.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Order {
    /// Order ID (UUID).
    pub id: String,

    /// Client-provided order ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_order_id: Option<String>,

    /// Instrument symbol.
    pub symbol: String,

    /// Order side.
    pub side: Side,

    /// Order type.
    pub order_type: OrderType,

    /// Total order quantity.
    pub quantity: Decimal,

    /// Quantity filled so far.
    pub filled_quantity: Decimal,

    /// Stop/trigger price (0 for market orders, `stop_price` for stop/stop-limit orders).
    pub price: Decimal,

    /// Limit price for stop-limit orders only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_price: Option<Decimal>,

    /// Current order status.
    pub status: OrderStatus,

    /// Position ID affected by this order (for hedging mode tracking).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_id: Option<String>,

    /// Order creation timestamp (Unix ms).
    pub created_at: u64,

    /// Fill timestamp (Unix ms).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executed_at: Option<u64>,

    /// Cancellation timestamp (Unix ms).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancelled_at: Option<u64>,

    /// Rejection reason (only set when status is `rejected`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reject_reason: Option<String>,

    /// Rejection timestamp (Unix ms, only set when status is `rejected`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejected_at: Option<u64>,
}

/// Trade (execution) details.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trade {
    /// Trade ID.
    pub id: String,

    /// Associated order ID.
    pub order_id: String,

    /// Position ID this trade affected (for hedging mode tracking).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_id: Option<String>,

    /// Instrument symbol.
    pub symbol: String,

    /// Trade side.
    pub side: Side,

    /// Fill quantity.
    pub quantity: Decimal,

    /// Fill price (after slippage was applied).
    pub price: Decimal,

    /// Base market price before slippage was applied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_price: Option<Decimal>,

    /// Slippage applied in basis points (e.g., 5 = 0.05%).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slippage_bps: Option<Decimal>,

    /// Commission/fee for this trade.
    pub commission: Decimal,

    /// Realized P&L if closing a position (null for opening trades).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub realized_pnl: Option<Decimal>,

    /// Fill timestamp (Unix ms).
    pub timestamp: u64,

    /// Whether this trade was a system-initiated liquidation.
    pub liquidation: bool,
}

/// Position details.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    /// Position ID.
    pub id: String,

    /// Instrument symbol.
    pub symbol: String,

    /// Position side (long or short).
    pub side: PositionSide,

    /// Position size.
    pub quantity: Decimal,

    /// Average entry price.
    pub avg_entry_price: Decimal,

    /// Current market price.
    pub current_price: Decimal,

    /// Unrealized P&L.
    pub unrealized_pnl: Decimal,
}

/// Account state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// Cash balance (starting capital + realized P&L - commissions).
    pub balance: Decimal,

    /// Total account value (balance + unrealized P&L).
    pub equity: Decimal,

    /// Unrealized profit/loss from open positions.
    pub unrealized_pnl: Decimal,

    /// Cumulative realized P&L from closed trades.
    pub realized_pnl: Decimal,

    /// Cumulative commission paid.
    pub total_commission: Decimal,

    /// Margin consumed by positions.
    pub margin_used: Decimal,

    /// Free margin for new positions.
    pub margin_available: Decimal,
}

/// Result of cancel all orders operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelAllResult {
    /// Number of orders cancelled.
    pub cancelled_count: u32,

    /// Number of orders that failed to cancel.
    pub failed_count: u32,
}

// =============================================================================
// Response Wrappers
// =============================================================================

/// Response containing a list of orders.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrdersResponse {
    pub orders: Vec<Order>,
}

/// Response containing a list of trades.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradesResponse {
    pub trades: Vec<Trade>,
}

/// Response containing a list of positions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PositionsResponse {
    pub positions: Vec<Position>,
}

// =============================================================================
// Bars Types
// =============================================================================

/// Query parameters for getting historical bars.
#[derive(Debug, Clone, Deserialize)]
pub struct GetBarsQueryParams {
    /// Instrument symbol (e.g., "F:EURUSD").
    pub symbol: String,

    /// Candle timeframe (e.g., "1m", "5m", "1h").
    pub timeframe: String,

    /// Number of bars to return (max 10,000).
    pub count: u32,
}

/// Historical bar (OHLCV) data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bar {
    /// Bar timestamp (Unix milliseconds).
    pub timestamp: u64,

    /// Open price.
    pub open: Decimal,

    /// High price.
    pub high: Decimal,

    /// Low price.
    pub low: Decimal,

    /// Close price.
    pub close: Decimal,

    /// Volume.
    pub volume: f64,
}

/// Response containing historical bars.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BarsResponse {
    pub bars: Vec<Bar>,
}

// =============================================================================
// Quote Types
// =============================================================================

/// Query parameters for getting a quote.
#[derive(Debug, Clone, Deserialize)]
pub struct GetQuoteQueryParams {
    /// Instrument symbol (e.g., "F:EURUSD").
    pub symbol: String,
}

/// Current quote (bid/ask) for a symbol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quote {
    /// Instrument symbol.
    pub symbol: String,

    /// Current bid price.
    pub bid: Decimal,

    /// Current ask price.
    pub ask: Decimal,

    /// Quote timestamp (Unix milliseconds).
    pub timestamp: u64,
}

// =============================================================================
// Symbol Info Types
// =============================================================================

/// Information about a tradeable symbol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolInfo {
    /// Symbol identifier (e.g., "F:EURUSD").
    pub symbol: String,

    /// Human-readable name.
    pub name: String,

    /// Asset class/market type (e.g., "crypto", "forex", "stocks").
    pub asset_class: String,

    /// Whether the symbol is currently tradeable.
    pub tradeable: bool,
}

/// Response containing a list of symbols.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolsResponse {
    pub symbols: Vec<SymbolInfo>,
}
