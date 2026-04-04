//! Bracket and OCO (One-Cancels-Other) order types.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::Side;

/// Request to place a new OCO order pair (stop-loss + take-profit).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub struct PlaceOcoOrderRequest {
    /// Trading symbol
    pub symbol: String,

    /// Order side (Sell for exits from long, Buy for exits from short)
    pub side: Side,

    /// Order quantity
    pub quantity: rust_decimal::Decimal,

    /// Take-profit limit price (the favorable exit)
    pub take_profit_price: rust_decimal::Decimal,

    /// Stop-loss trigger price (the adverse exit)
    pub stop_loss_trigger: rust_decimal::Decimal,

    /// Optional stop-limit price (limit price after stop triggers).
    /// If not provided, defaults to `stop_loss_trigger` (stop-market behavior).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_loss_limit: Option<rust_decimal::Decimal>,
}

/// Response from placing an OCO order pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub struct PlaceOcoOrderResponse {
    /// OCO group identifier
    pub list_order_id: String,

    /// Individual stop-loss order ID
    pub stop_loss_order_id: String,

    /// Individual take-profit order ID
    pub take_profit_order_id: String,

    /// Trading symbol
    pub symbol: String,

    /// OCO status: "active", "filled", "cancelled"
    pub status: String,
}
