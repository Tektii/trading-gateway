//! Order-related models.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use validator::Validate;

use super::{MarginMode, OrderStatus, OrderType, RejectReason, Side, TimeInForce, TrailingType};
use crate::error::GatewayError;

/// Request to submit a new order.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Validate)]
#[serde(rename_all = "snake_case")]
pub struct OrderRequest {
    /// Canonical symbol format (e.g., "EUR/USD", "AAPL")
    #[validate(length(min = 1, max = 20))]
    pub symbol: String,

    /// Buy or sell
    pub side: Side,

    /// Quantity in base units (must be positive)
    #[validate(custom(function = "validate_positive_decimal"))]
    pub quantity: Decimal,

    /// Order type
    pub order_type: OrderType,

    /// Required for LIMIT and `STOP_LIMIT` orders
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_price: Option<Decimal>,

    /// Required for STOP and `STOP_LIMIT` orders
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_price: Option<Decimal>,

    /// Distance for `TRAILING_STOP` orders
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trailing_distance: Option<Decimal>,

    /// Type of trailing stop (absolute or percent)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trailing_type: Option<TrailingType>,

    /// Stop loss price for bracket order
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_loss: Option<Decimal>,

    /// Take profit price for bracket order
    #[serde(skip_serializing_if = "Option::is_none")]
    pub take_profit: Option<Decimal>,

    /// Target position ID (for hedging accounts)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_id: Option<String>,

    /// If true, can only reduce position
    #[serde(default)]
    pub reduce_only: bool,

    /// If true, reject if would immediately fill (maker only)
    #[serde(default)]
    pub post_only: bool,

    /// If true, order not visible in order book
    #[serde(default)]
    pub hidden: bool,

    /// Visible quantity for iceberg orders
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_quantity: Option<Decimal>,

    /// Margin mode (cross or isolated)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub margin_mode: Option<MarginMode>,

    /// Leverage multiplier
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leverage: Option<Decimal>,

    /// Client-provided order ID for idempotency
    #[validate(length(max = 64))]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_order_id: Option<String>,

    /// OCO group ID to link orders (one-cancels-other).
    #[validate(length(max = 64))]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oco_group_id: Option<String>,

    /// Time in force (default: GTC)
    #[serde(default)]
    pub time_in_force: TimeInForce,
}

/// Validation helper for positive decimals
fn validate_positive_decimal(value: &Decimal) -> Result<(), validator::ValidationError> {
    if *value <= Decimal::ZERO {
        return Err(validator::ValidationError::new("must be positive"));
    }
    Ok(())
}

impl OrderRequest {
    /// Create a market order
    #[must_use]
    pub fn market(symbol: impl Into<String>, side: Side, quantity: Decimal) -> Self {
        Self {
            symbol: symbol.into(),
            side,
            quantity,
            order_type: OrderType::Market,
            limit_price: None,
            stop_price: None,
            trailing_distance: None,
            trailing_type: None,
            stop_loss: None,
            take_profit: None,
            position_id: None,
            reduce_only: false,
            post_only: false,
            hidden: false,
            display_quantity: None,
            margin_mode: None,
            leverage: None,
            client_order_id: None,
            oco_group_id: None,
            time_in_force: TimeInForce::default(),
        }
    }

    /// Create a limit order
    #[must_use]
    pub fn limit(symbol: impl Into<String>, side: Side, quantity: Decimal, price: Decimal) -> Self {
        Self {
            order_type: OrderType::Limit,
            limit_price: Some(price),
            ..Self::market(symbol, side, quantity)
        }
    }

    /// Create a stop order
    #[must_use]
    pub fn stop(
        symbol: impl Into<String>,
        side: Side,
        quantity: Decimal,
        stop_price: Decimal,
    ) -> Self {
        Self {
            order_type: OrderType::Stop,
            stop_price: Some(stop_price),
            ..Self::market(symbol, side, quantity)
        }
    }

    /// Create a stop-limit order
    #[must_use]
    pub fn stop_limit(
        symbol: impl Into<String>,
        side: Side,
        quantity: Decimal,
        stop_price: Decimal,
        limit_price: Decimal,
    ) -> Self {
        Self {
            order_type: OrderType::StopLimit,
            stop_price: Some(stop_price),
            limit_price: Some(limit_price),
            ..Self::market(symbol, side, quantity)
        }
    }

    /// Add stop loss to this order (bracket order)
    #[must_use]
    pub const fn with_stop_loss(mut self, price: Decimal) -> Self {
        self.stop_loss = Some(price);
        self
    }

    /// Add take profit to this order (bracket order)
    #[must_use]
    pub const fn with_take_profit(mut self, price: Decimal) -> Self {
        self.take_profit = Some(price);
        self
    }

    /// Set reduce-only flag
    #[must_use]
    pub const fn reduce_only(mut self) -> Self {
        self.reduce_only = true;
        self
    }

    /// Set post-only flag (maker only)
    #[must_use]
    pub const fn post_only(mut self) -> Self {
        self.post_only = true;
        self
    }

    /// Set hidden flag
    #[must_use]
    pub const fn hidden(mut self) -> Self {
        self.hidden = true;
        self
    }

    /// Set client order ID for idempotency
    #[must_use]
    pub fn with_client_id(mut self, id: impl Into<String>) -> Self {
        self.client_order_id = Some(id.into());
        self
    }

    /// Set OCO group ID to link with another order.
    #[must_use]
    pub fn with_oco_group(mut self, group_id: impl Into<String>) -> Self {
        self.oco_group_id = Some(group_id.into());
        self
    }

    /// Set position ID (for hedging accounts)
    #[must_use]
    pub fn for_position(mut self, position_id: impl Into<String>) -> Self {
        self.position_id = Some(position_id.into());
        self
    }

    /// Set margin mode
    #[must_use]
    pub const fn with_margin_mode(mut self, mode: MarginMode) -> Self {
        self.margin_mode = Some(mode);
        self
    }

    /// Set leverage
    #[must_use]
    pub const fn with_leverage(mut self, leverage: Decimal) -> Self {
        self.leverage = Some(leverage);
        self
    }

    /// Set display quantity for iceberg orders
    #[must_use]
    pub const fn with_display_quantity(mut self, qty: Decimal) -> Self {
        self.display_quantity = Some(qty);
        self
    }

    /// Set time in force
    #[must_use]
    pub const fn with_time_in_force(mut self, tif: TimeInForce) -> Self {
        self.time_in_force = tif;
        self
    }

    /// Validate cross-field semantics: order type vs required/forbidden price fields,
    /// bracket leg prices, display quantity, and leverage.
    pub fn validate_semantics(&self) -> Result<(), GatewayError> {
        match self.order_type {
            OrderType::Market => {
                reject_unexpected("Market", "limit_price", self.limit_price.as_ref())?;
                reject_unexpected("Market", "stop_price", self.stop_price.as_ref())?;
                reject_unexpected(
                    "Market",
                    "trailing_distance",
                    self.trailing_distance.as_ref(),
                )?;
            }
            OrderType::Limit => {
                require_positive("Limit", "limit_price", self.limit_price.as_ref())?;
                reject_unexpected("Limit", "stop_price", self.stop_price.as_ref())?;
                reject_unexpected(
                    "Limit",
                    "trailing_distance",
                    self.trailing_distance.as_ref(),
                )?;
            }
            OrderType::Stop => {
                require_positive("Stop", "stop_price", self.stop_price.as_ref())?;
                reject_unexpected("Stop", "limit_price", self.limit_price.as_ref())?;
                reject_unexpected("Stop", "trailing_distance", self.trailing_distance.as_ref())?;
            }
            OrderType::StopLimit => {
                require_positive("StopLimit", "stop_price", self.stop_price.as_ref())?;
                require_positive("StopLimit", "limit_price", self.limit_price.as_ref())?;
                reject_unexpected(
                    "StopLimit",
                    "trailing_distance",
                    self.trailing_distance.as_ref(),
                )?;
            }
            OrderType::TrailingStop => {
                let distance = require_positive(
                    "TrailingStop",
                    "trailing_distance",
                    self.trailing_distance.as_ref(),
                )?;
                if self.trailing_type.is_none() {
                    return Err(missing_field("TrailingStop", "trailing_type"));
                }
                if self.trailing_type == Some(TrailingType::Percent)
                    && distance >= Decimal::from(100)
                {
                    return Err(GatewayError::InvalidValue {
                        field: "trailing_distance".to_string(),
                        message: "percent trailing distance must be less than 100".to_string(),
                        provided: Some(distance.to_string()),
                    });
                }
                reject_unexpected("TrailingStop", "limit_price", self.limit_price.as_ref())?;
                reject_unexpected("TrailingStop", "stop_price", self.stop_price.as_ref())?;
            }
        }

        // Cross-cutting validations
        if let Some(sl) = self.stop_loss
            && sl <= Decimal::ZERO
        {
            return Err(GatewayError::InvalidValue {
                field: "stop_loss".to_string(),
                message: "must be positive".to_string(),
                provided: Some(sl.to_string()),
            });
        }
        if let Some(tp) = self.take_profit
            && tp <= Decimal::ZERO
        {
            return Err(GatewayError::InvalidValue {
                field: "take_profit".to_string(),
                message: "must be positive".to_string(),
                provided: Some(tp.to_string()),
            });
        }
        if let Some(dq) = self.display_quantity {
            if dq <= Decimal::ZERO {
                return Err(GatewayError::InvalidValue {
                    field: "display_quantity".to_string(),
                    message: "must be positive".to_string(),
                    provided: Some(dq.to_string()),
                });
            }
            if dq >= self.quantity {
                return Err(GatewayError::InvalidValue {
                    field: "display_quantity".to_string(),
                    message: "must be less than quantity".to_string(),
                    provided: Some(dq.to_string()),
                });
            }
        }
        if let Some(lev) = self.leverage
            && lev <= Decimal::ZERO
        {
            return Err(GatewayError::InvalidValue {
                field: "leverage".to_string(),
                message: "must be positive".to_string(),
                provided: Some(lev.to_string()),
            });
        }

        Ok(())
    }
}

/// Return `InvalidRequest` for a missing required field.
fn missing_field(order_type: &str, field: &str) -> GatewayError {
    GatewayError::InvalidRequest {
        message: format!("{order_type} orders require {field}"),
        field: Some(field.to_string()),
    }
}

/// Reject an unexpected field that should not be set for this order type.
fn reject_unexpected(
    order_type: &str,
    field: &str,
    value: Option<&Decimal>,
) -> Result<(), GatewayError> {
    if let Some(v) = value {
        return Err(GatewayError::InvalidValue {
            field: field.to_string(),
            message: format!("{order_type} orders should not have {field}"),
            provided: Some(v.to_string()),
        });
    }
    Ok(())
}

/// Require a field to be present and positive. Returns the value on success.
fn require_positive(
    order_type: &str,
    field: &str,
    value: Option<&Decimal>,
) -> Result<Decimal, GatewayError> {
    match value {
        None => Err(missing_field(order_type, field)),
        Some(v) if *v <= Decimal::ZERO => Err(GatewayError::InvalidValue {
            field: field.to_string(),
            message: "must be positive".to_string(),
            provided: Some(v.to_string()),
        }),
        Some(v) => Ok(*v),
    }
}

/// Minimal response after order submission.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub struct OrderHandle {
    /// Server-assigned order ID
    pub id: String,

    /// Client-provided order ID (if any)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_order_id: Option<String>,

    /// Gateway-assigned correlation ID for tracing this order through its lifecycle.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,

    /// Initial order status
    pub status: OrderStatus,
}

/// Full order details.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub struct Order {
    /// Server-assigned order ID
    pub id: String,

    /// Client-provided order ID (if any)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_order_id: Option<String>,

    /// Trading symbol
    pub symbol: String,

    /// Order side
    pub side: Side,

    /// Order type
    pub order_type: OrderType,

    /// Original order quantity
    pub quantity: Decimal,

    /// Quantity filled so far
    pub filled_quantity: Decimal,

    /// Quantity remaining
    pub remaining_quantity: Decimal,

    /// Limit price (for LIMIT and `STOP_LIMIT` orders)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_price: Option<Decimal>,

    /// Stop price (for STOP and `STOP_LIMIT` orders)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_price: Option<Decimal>,

    /// Stop loss price (for bracket orders)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_loss: Option<Decimal>,

    /// Take profit price (for bracket orders)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub take_profit: Option<Decimal>,

    /// Trailing distance (for trailing stop orders)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trailing_distance: Option<Decimal>,

    /// Trailing type (absolute or percent)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trailing_type: Option<TrailingType>,

    /// Weighted average fill price
    #[serde(skip_serializing_if = "Option::is_none")]
    pub average_fill_price: Option<Decimal>,

    /// Current order status
    pub status: OrderStatus,

    /// Rejection reason (if status is REJECTED)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reject_reason: Option<RejectReason>,

    /// Associated position ID (for hedging accounts)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_id: Option<String>,

    /// Whether this is a reduce-only order
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reduce_only: Option<bool>,

    /// Whether this is a post-only order
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post_only: Option<bool>,

    /// Whether this is a hidden order
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,

    /// Display quantity for iceberg orders
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_quantity: Option<Decimal>,

    /// OCO group ID if this order is part of an OCO pair.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oco_group_id: Option<String>,

    /// Gateway-assigned correlation ID for tracing this order through its lifecycle.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,

    /// Time in force
    pub time_in_force: TimeInForce,

    /// When the order was created
    pub created_at: DateTime<Utc>,

    /// When the order was last updated
    pub updated_at: DateTime<Utc>,
}

/// Request to modify an existing order.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema, Validate)]
#[serde(rename_all = "snake_case")]
pub struct ModifyOrderRequest {
    /// New limit price
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_price: Option<Decimal>,

    /// New stop price
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_price: Option<Decimal>,

    /// New total quantity (must be >= `filled_quantity`)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quantity: Option<Decimal>,

    /// New stop loss price
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_loss: Option<Decimal>,

    /// New take profit price
    #[serde(skip_serializing_if = "Option::is_none")]
    pub take_profit: Option<Decimal>,

    /// New trailing distance
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trailing_distance: Option<Decimal>,
}

/// Result of modifying an order.
///
/// Some providers (e.g., Alpaca) implement modify as cancel-replace,
/// which may result in a new order ID.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub struct ModifyOrderResult {
    /// The modified (or replacement) order
    pub order: Order,

    /// Previous order ID if the provider used cancel-replace.
    /// `None` if the order was modified in-place.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_order_id: Option<String>,
}

/// Query parameters for listing orders.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema, utoipa::IntoParams)]
#[serde(rename_all = "snake_case")]
pub struct OrderQueryParams {
    /// Filter by symbol
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,

    /// Filter by status (multiple allowed, accepts comma-separated values)
    #[serde(
        skip_serializing_if = "Option::is_none",
        default,
        deserialize_with = "deserialize_comma_separated_status"
    )]
    pub status: Option<Vec<OrderStatus>>,

    /// Filter by side
    #[serde(skip_serializing_if = "Option::is_none")]
    pub side: Option<Side>,

    /// Filter by client order ID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_order_id: Option<String>,

    /// Filter by OCO group ID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oco_group_id: Option<String>,

    /// Filter orders created after this time
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<DateTime<Utc>>,

    /// Filter orders created before this time
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<DateTime<Utc>>,

    /// Maximum number of orders to return
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// Deserialize status field accepting both repeated params (`?status=X&status=Y`)
/// and comma-separated values (`?status=X,Y`).
fn deserialize_comma_separated_status<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<OrderStatus>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    // Try to deserialize as a sequence first (repeated params), fall back to string
    let raw: Option<super::OneOrMany<String>> = Option::deserialize(deserializer)?;
    match raw {
        None => Ok(None),
        Some(super::OneOrMany::One(s)) => {
            let statuses: Result<Vec<OrderStatus>, _> = s
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| serde_json::from_value(serde_json::Value::String(s.to_string())))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| D::Error::custom(format!("invalid status value: {e}")));
            let statuses = statuses?;
            if statuses.is_empty() {
                Ok(None)
            } else {
                Ok(Some(statuses))
            }
        }
        Some(super::OneOrMany::Many(values)) => {
            let statuses: Result<Vec<OrderStatus>, _> = values
                .into_iter()
                .flat_map(|s| s.split(',').map(str::to_string).collect::<Vec<_>>())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .map(|s| serde_json::from_value(serde_json::Value::String(s)))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| D::Error::custom(format!("invalid status value: {e}")));
            let statuses = statuses?;
            if statuses.is_empty() {
                Ok(None)
            } else {
                Ok(Some(statuses))
            }
        }
    }
}

/// Result of cancelling an order.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub struct CancelOrderResult {
    /// Whether cancellation succeeded
    pub success: bool,

    /// Updated order state
    pub order: Order,
}

/// Result of cancelling multiple orders.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub struct CancelAllResult {
    /// Number of orders successfully cancelled
    pub cancelled_count: u32,

    /// Number of orders that failed to cancel
    pub failed_count: u32,

    /// IDs of orders that failed to cancel
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_order_ids: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;
    use crate::error::GatewayError;

    fn base_market() -> OrderRequest {
        OrderRequest::market("AAPL", Side::Buy, dec!(10))
    }

    // --- Market order ---

    #[test]
    fn validate_semantics_market_valid() {
        assert!(base_market().validate_semantics().is_ok());
    }

    #[test]
    fn validate_semantics_market_rejects_limit_price() {
        let mut o = base_market();
        o.limit_price = Some(dec!(100));
        let err = o.validate_semantics().unwrap_err();
        assert!(
            matches!(err, GatewayError::InvalidValue { ref field, .. } if field == "limit_price")
        );
    }

    #[test]
    fn validate_semantics_market_rejects_stop_price() {
        let mut o = base_market();
        o.stop_price = Some(dec!(100));
        let err = o.validate_semantics().unwrap_err();
        assert!(
            matches!(err, GatewayError::InvalidValue { ref field, .. } if field == "stop_price")
        );
    }

    #[test]
    fn validate_semantics_market_rejects_trailing_distance() {
        let mut o = base_market();
        o.trailing_distance = Some(dec!(5));
        let err = o.validate_semantics().unwrap_err();
        assert!(
            matches!(err, GatewayError::InvalidValue { ref field, .. } if field == "trailing_distance")
        );
    }

    // --- Limit order ---

    #[test]
    fn validate_semantics_limit_valid() {
        assert!(
            OrderRequest::limit("AAPL", Side::Buy, dec!(10), dec!(150))
                .validate_semantics()
                .is_ok()
        );
    }

    #[test]
    fn validate_semantics_limit_missing_price() {
        let mut o = base_market();
        o.order_type = OrderType::Limit;
        let err = o.validate_semantics().unwrap_err();
        assert!(
            matches!(err, GatewayError::InvalidRequest { ref field, .. } if field.as_deref() == Some("limit_price"))
        );
    }

    #[test]
    fn validate_semantics_limit_zero_price() {
        let o = OrderRequest::limit("AAPL", Side::Buy, dec!(10), dec!(0));
        let err = o.validate_semantics().unwrap_err();
        assert!(
            matches!(err, GatewayError::InvalidValue { ref field, .. } if field == "limit_price")
        );
    }

    #[test]
    fn validate_semantics_limit_rejects_stop_price() {
        let mut o = OrderRequest::limit("AAPL", Side::Buy, dec!(10), dec!(150));
        o.stop_price = Some(dec!(140));
        let err = o.validate_semantics().unwrap_err();
        assert!(
            matches!(err, GatewayError::InvalidValue { ref field, .. } if field == "stop_price")
        );
    }

    // --- Stop order ---

    #[test]
    fn validate_semantics_stop_valid() {
        assert!(
            OrderRequest::stop("AAPL", Side::Sell, dec!(10), dec!(140))
                .validate_semantics()
                .is_ok()
        );
    }

    #[test]
    fn validate_semantics_stop_missing_price() {
        let mut o = base_market();
        o.order_type = OrderType::Stop;
        let err = o.validate_semantics().unwrap_err();
        assert!(
            matches!(err, GatewayError::InvalidRequest { ref field, .. } if field.as_deref() == Some("stop_price"))
        );
    }

    #[test]
    fn validate_semantics_stop_rejects_limit_price() {
        let mut o = OrderRequest::stop("AAPL", Side::Sell, dec!(10), dec!(140));
        o.limit_price = Some(dec!(135));
        let err = o.validate_semantics().unwrap_err();
        assert!(
            matches!(err, GatewayError::InvalidValue { ref field, .. } if field == "limit_price")
        );
    }

    // --- StopLimit order ---

    #[test]
    fn validate_semantics_stop_limit_valid() {
        assert!(
            OrderRequest::stop_limit("AAPL", Side::Buy, dec!(10), dec!(140), dec!(145))
                .validate_semantics()
                .is_ok()
        );
    }

    #[test]
    fn validate_semantics_stop_limit_missing_stop_price() {
        let mut o = OrderRequest::stop_limit("AAPL", Side::Buy, dec!(10), dec!(140), dec!(145));
        o.stop_price = None;
        let err = o.validate_semantics().unwrap_err();
        assert!(
            matches!(err, GatewayError::InvalidRequest { ref field, .. } if field.as_deref() == Some("stop_price"))
        );
    }

    #[test]
    fn validate_semantics_stop_limit_missing_limit_price() {
        let mut o = OrderRequest::stop_limit("AAPL", Side::Buy, dec!(10), dec!(140), dec!(145));
        o.limit_price = None;
        let err = o.validate_semantics().unwrap_err();
        assert!(
            matches!(err, GatewayError::InvalidRequest { ref field, .. } if field.as_deref() == Some("limit_price"))
        );
    }

    // --- TrailingStop order ---

    fn trailing_stop_order() -> OrderRequest {
        let mut o = base_market();
        o.order_type = OrderType::TrailingStop;
        o.trailing_distance = Some(dec!(5));
        o.trailing_type = Some(TrailingType::Absolute);
        o
    }

    #[test]
    fn validate_semantics_trailing_stop_valid() {
        assert!(trailing_stop_order().validate_semantics().is_ok());
    }

    #[test]
    fn validate_semantics_trailing_stop_missing_distance() {
        let mut o = trailing_stop_order();
        o.trailing_distance = None;
        let err = o.validate_semantics().unwrap_err();
        assert!(
            matches!(err, GatewayError::InvalidRequest { ref field, .. } if field.as_deref() == Some("trailing_distance"))
        );
    }

    #[test]
    fn validate_semantics_trailing_stop_missing_type() {
        let mut o = trailing_stop_order();
        o.trailing_type = None;
        let err = o.validate_semantics().unwrap_err();
        assert!(
            matches!(err, GatewayError::InvalidRequest { ref field, .. } if field.as_deref() == Some("trailing_type"))
        );
    }

    #[test]
    fn validate_semantics_trailing_stop_percent_at_100() {
        let mut o = trailing_stop_order();
        o.trailing_distance = Some(dec!(100));
        o.trailing_type = Some(TrailingType::Percent);
        let err = o.validate_semantics().unwrap_err();
        assert!(
            matches!(err, GatewayError::InvalidValue { ref field, .. } if field == "trailing_distance")
        );
    }

    #[test]
    fn validate_semantics_trailing_stop_percent_99_valid() {
        let mut o = trailing_stop_order();
        o.trailing_distance = Some(dec!(99.9));
        o.trailing_type = Some(TrailingType::Percent);
        assert!(o.validate_semantics().is_ok());
    }

    #[test]
    fn validate_semantics_trailing_stop_rejects_limit_price() {
        let mut o = trailing_stop_order();
        o.limit_price = Some(dec!(100));
        let err = o.validate_semantics().unwrap_err();
        assert!(
            matches!(err, GatewayError::InvalidValue { ref field, .. } if field == "limit_price")
        );
    }

    #[test]
    fn validate_semantics_trailing_stop_rejects_stop_price() {
        let mut o = trailing_stop_order();
        o.stop_price = Some(dec!(100));
        let err = o.validate_semantics().unwrap_err();
        assert!(
            matches!(err, GatewayError::InvalidValue { ref field, .. } if field == "stop_price")
        );
    }

    // --- Cross-cutting ---

    #[test]
    fn validate_semantics_stop_loss_must_be_positive() {
        let mut o = base_market();
        o.stop_loss = Some(dec!(-1));
        let err = o.validate_semantics().unwrap_err();
        assert!(
            matches!(err, GatewayError::InvalidValue { ref field, .. } if field == "stop_loss")
        );
    }

    #[test]
    fn validate_semantics_take_profit_must_be_positive() {
        let mut o = base_market();
        o.take_profit = Some(dec!(0));
        let err = o.validate_semantics().unwrap_err();
        assert!(
            matches!(err, GatewayError::InvalidValue { ref field, .. } if field == "take_profit")
        );
    }

    #[test]
    fn validate_semantics_bracket_legs_valid() {
        let o = base_market()
            .with_stop_loss(dec!(140))
            .with_take_profit(dec!(160));
        assert!(o.validate_semantics().is_ok());
    }

    #[test]
    fn validate_semantics_display_quantity_must_be_positive() {
        let o = base_market().with_display_quantity(dec!(0));
        let err = o.validate_semantics().unwrap_err();
        assert!(
            matches!(err, GatewayError::InvalidValue { ref field, .. } if field == "display_quantity")
        );
    }

    #[test]
    fn validate_semantics_display_quantity_must_be_less_than_quantity() {
        let o = base_market().with_display_quantity(dec!(15));
        let err = o.validate_semantics().unwrap_err();
        assert!(
            matches!(err, GatewayError::InvalidValue { ref field, ref message, .. } if field == "display_quantity" && message.contains("less than"))
        );
    }

    #[test]
    fn validate_semantics_display_quantity_valid() {
        let o = base_market().with_display_quantity(dec!(5));
        assert!(o.validate_semantics().is_ok());
    }

    #[test]
    fn validate_semantics_leverage_must_be_positive() {
        let o = base_market().with_leverage(dec!(0));
        let err = o.validate_semantics().unwrap_err();
        assert!(matches!(err, GatewayError::InvalidValue { ref field, .. } if field == "leverage"));
    }
}
