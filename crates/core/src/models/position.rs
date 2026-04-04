//! Position-related models.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use validator::Validate;

use super::{MarginMode, OrderType, PositionSide};

/// Position details.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub struct Position {
    /// Position ID (may be synthetic for stock brokers)
    pub id: String,

    /// Trading symbol
    pub symbol: String,

    /// Position direction (long or short)
    pub side: PositionSide,

    /// Current position quantity
    pub quantity: Decimal,

    /// Weighted average entry price
    pub average_entry_price: Decimal,

    /// Current market price
    pub current_price: Decimal,

    /// Unrealized profit/loss
    pub unrealized_pnl: Decimal,

    /// Realized profit/loss (from partial closes)
    pub realized_pnl: Decimal,

    /// Margin mode (for leveraged positions)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub margin_mode: Option<MarginMode>,

    /// Leverage multiplier
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leverage: Option<Decimal>,

    /// Liquidation price (for leveraged positions)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub liquidation_price: Option<Decimal>,

    /// When the position was opened
    pub opened_at: DateTime<Utc>,

    /// When the position was last modified
    pub updated_at: DateTime<Utc>,
}

/// Request to close a position.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema, Validate)]
#[serde(rename_all = "snake_case")]
pub struct ClosePositionRequest {
    /// Order type for close (default: MARKET)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_type: Option<OrderType>,

    /// Limit price (required if `order_type` is LIMIT)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_price: Option<Decimal>,

    /// Quantity to close (None = close entire position)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quantity: Option<Decimal>,

    /// Cancel pending SL/TP orders for this position (default: true)
    #[serde(default = "default_true")]
    pub cancel_associated_orders: bool,
}

/// Default value helper for serde
const fn default_true() -> bool {
    true
}

/// Result of closing all positions.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub struct CloseAllPositionsResult {
    /// Order handles for each closing order created
    pub orders: Vec<super::OrderHandle>,
}
