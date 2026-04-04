//! Quote model for real-time market data.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Real-time quote with bid/ask/last prices.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub struct Quote {
    /// Symbol in canonical format (e.g., "EUR/USD", "AAPL")
    pub symbol: String,

    /// Data source provider (e.g., "alpaca", "binance")
    pub provider: String,

    /// Best bid price (highest buy offer)
    pub bid: Decimal,

    /// Quantity available at bid price
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bid_size: Option<Decimal>,

    /// Best ask price (lowest sell offer)
    pub ask: Decimal,

    /// Quantity available at ask price
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ask_size: Option<Decimal>,

    /// Last traded price
    pub last: Decimal,

    /// Trading volume (if available)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<Decimal>,

    /// Quote timestamp
    pub timestamp: DateTime<Utc>,
}
