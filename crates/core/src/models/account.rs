//! Account information models.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Trading account information.
///
/// Represents the current state of a trading account including cash balance,
/// equity (balance + unrealized P&L), and margin usage. All monetary values
/// are in the account's base currency.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub struct Account {
    /// Cash balance (excluding unrealized P&L).
    pub balance: Decimal,

    /// Total equity (balance + unrealized P&L).
    pub equity: Decimal,

    /// Margin currently in use for open positions.
    pub margin_used: Decimal,

    /// Available margin for new positions.
    pub margin_available: Decimal,

    /// Total unrealized P&L across all open positions.
    pub unrealized_pnl: Decimal,

    /// Account base currency (e.g., "USD", "EUR").
    pub currency: String,
}
