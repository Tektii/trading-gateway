//! Trade/fill record types.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::Side;

/// Individual trade/fill record.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub struct Trade {
    /// Unique trade identifier from provider
    pub id: String,

    /// Order ID this trade belongs to
    pub order_id: String,

    /// Symbol traded
    pub symbol: String,

    /// Trade side (buy or sell)
    pub side: Side,

    /// Fill quantity in base units
    pub quantity: Decimal,

    /// Fill price
    pub price: Decimal,

    /// Commission/fee amount for this trade
    pub commission: Decimal,

    /// Currency of the commission (e.g., "USD", "USDT", "BNB")
    pub commission_currency: String,

    /// Whether this trade was a maker (added liquidity) or taker.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_maker: Option<bool>,

    /// Trade execution timestamp
    pub timestamp: DateTime<Utc>,
}

/// Query parameters for listing trades.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema, utoipa::IntoParams)]
#[serde(rename_all = "snake_case")]
pub struct TradeQueryParams {
    /// Filter by symbol
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,

    /// Filter by order ID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_id: Option<String>,

    /// Return trades after this time (inclusive)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<DateTime<Utc>>,

    /// Return trades before this time (exclusive)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<DateTime<Utc>>,

    /// Maximum number of trades to return
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}
