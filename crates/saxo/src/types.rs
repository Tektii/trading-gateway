//! Saxo Bank API type definitions.
//!
//! Auth DTOs (token request/response) and REST DTOs for orders, positions,
//! quotes, bars, and trade history.
#![allow(dead_code)] // Broker API response types — all fields required for deserialization

use serde::{Deserialize, Serialize};

// ============================================================================
// Auth Types
// ============================================================================

/// Successful token response from `POST /connect/token`.
#[derive(Debug, Deserialize)]
pub struct SaxoTokenResponse {
    /// The access token for API requests.
    pub access_token: String,
    /// Token type (always "Bearer").
    pub token_type: String,
    /// Seconds until the access token expires.
    pub expires_in: u64,
    /// Refresh token for obtaining new access tokens (may be absent for dev portal tokens).
    pub refresh_token: Option<String>,
    /// Seconds until the refresh token expires (may be absent).
    pub refresh_token_expires_in: Option<u64>,
}

/// Error response from the token endpoint.
#[derive(Debug, Deserialize)]
pub struct SaxoTokenErrorResponse {
    /// OAuth2 error code (e.g., "invalid_grant").
    pub error: String,
    /// Human-readable error description.
    pub error_description: Option<String>,
}

// ============================================================================
// Account Types
// ============================================================================

/// Response from `GET /port/v1/balances/me`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SaxoBalanceResponse {
    /// Total account value (balance + unrealized P&L).
    #[serde(default)]
    pub total_value: f64,
    /// Cash balance.
    #[serde(default)]
    pub cash_balance: f64,
    /// Margin used by open positions.
    #[serde(default)]
    pub margin_used_by_current_positions: f64,
    /// Margin available for new trades.
    #[serde(default)]
    pub margin_available: f64,
    /// Unrealized P&L across open positions.
    #[serde(default)]
    pub unrealized_positions_value: f64,
    /// Account currency (e.g., "USD").
    #[serde(default)]
    pub currency: String,
}

/// Response from `GET /port/v1/clients/me`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SaxoClientResponse {
    /// Client key (used for trade history queries).
    pub client_key: String,
    /// Default account key.
    #[serde(default)]
    pub default_account_key: Option<String>,
}

// ============================================================================
// Order Types — Responses
// ============================================================================

/// Response from `GET /port/v1/orders/me`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SaxoOrdersResponse {
    /// List of orders.
    #[serde(default)]
    pub data: Vec<SaxoOrder>,
    /// Total count of matching orders.
    #[serde(default, rename = "__count")]
    pub count: Option<u32>,
}

/// A single Saxo order.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SaxoOrder {
    /// Order ID.
    pub order_id: String,
    /// Unique instrument identifier.
    pub uic: u32,
    /// Asset type (e.g., "FxSpot").
    pub asset_type: String,
    /// Buy or sell direction ("Buy" or "Sell").
    pub buy_sell: String,
    /// Order type ("Market", "Limit", "Stop", "StopLimit", "TrailingStop").
    /// The `/port/v1/orders/me` endpoint returns this as `OpenOrderType`.
    #[serde(alias = "OpenOrderType")]
    pub order_type: String,
    /// Order amount (unsigned).
    pub amount: f64,
    /// Filled amount.
    #[serde(default)]
    pub filled_amount: f64,
    /// Limit/trigger price.
    #[serde(default)]
    pub price: Option<f64>,
    /// Stop price (for `StopLimit` orders).
    #[serde(default)]
    pub stop_price: Option<f64>,
    /// Trailing stop distance.
    #[serde(default)]
    pub trailing_stop_distance_to_market: Option<f64>,
    /// Trailing stop step.
    #[serde(default)]
    pub trailing_stop_step: Option<f64>,
    /// Order status ("Working", "Filled", "Cancelled", "Rejected", "Expired").
    pub status: String,
    /// Duration (time-in-force) of the order.
    #[serde(default)]
    pub duration: Option<SaxoDuration>,
    /// Client-assigned reference (max 50 chars).
    #[serde(default)]
    pub external_reference: Option<String>,
    /// Related orders (SL/TP attached to this order).
    #[serde(default)]
    pub related_orders: Option<Vec<SaxoRelatedOrder>>,
    /// Related open orders (when entry order is already filled).
    #[serde(default)]
    pub related_open_orders: Option<Vec<SaxoRelatedOpenOrder>>,
    /// Average fill price.
    #[serde(default)]
    pub average_fill_price: Option<f64>,
}

/// Duration (time-in-force) on a Saxo order.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SaxoDuration {
    /// Duration type: "DayOrder", "GoodTillCancel", "ImmediateOrCancel", "FillOrKill".
    pub duration_type: String,
}

/// A related order attached to a working order (not yet filled).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SaxoRelatedOrder {
    /// Order type of the related order.
    pub order_type: String,
    /// Trigger price for the related order.
    #[serde(default)]
    pub price: Option<f64>,
    /// Trailing stop distance.
    #[serde(default)]
    pub trailing_stop_distance_to_market: Option<f64>,
}

/// A related open order (SL/TP already active after entry fill).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SaxoRelatedOpenOrder {
    /// Order ID of the related open order.
    pub order_id: String,
    /// Order type.
    pub order_type: String,
    /// Trigger price.
    #[serde(default)]
    pub price: Option<f64>,
    /// Current status.
    pub status: String,
}

// ============================================================================
// Order Types — Requests
// ============================================================================

/// Request body for `POST /trade/v2/orders`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct SaxoOrderRequest {
    /// Unique instrument identifier.
    pub uic: u32,
    /// Asset type (e.g., "FxSpot").
    pub asset_type: String,
    /// Buy or sell direction.
    pub buy_sell: String,
    /// Order amount (unsigned).
    pub amount: f64,
    /// Order type.
    pub order_type: String,
    /// MiFID II algorithmic trading flag (always `false` for API orders).
    pub manual_order: bool,
    /// Account key.
    pub account_key: String,
    /// Order duration (time-in-force).
    pub order_duration: SaxoOrderDuration,
    /// Limit/trigger price.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_price: Option<f64>,
    /// Stop price (for `StopLimit`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_limit_price: Option<f64>,
    /// Trailing stop distance.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trailing_stop_distance_to_market: Option<f64>,
    /// Trailing stop step.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trailing_stop_step: Option<f64>,
    /// Client-assigned reference (max 50 chars).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_reference: Option<String>,
    /// Related orders (SL/TP as bracket).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orders: Option<Vec<SaxoRelatedOrderRequest>>,
}

/// Duration for order requests.
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct SaxoOrderDuration {
    /// Duration type.
    pub duration_type: String,
}

/// Related order in a bracket (SL or TP).
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct SaxoRelatedOrderRequest {
    /// Order type ("StopIfTraded" for SL, "Limit" for TP).
    pub order_type: String,
    /// Trigger price.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_price: Option<f64>,
    /// Order duration.
    pub order_duration: SaxoOrderDuration,
    /// Buy or sell direction (opposite of parent for SL/TP).
    pub buy_sell: String,
    /// Amount.
    pub amount: f64,
}

/// Request body for `PATCH /trade/v2/orders`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct SaxoModifyOrderRequest {
    /// Account key.
    pub account_key: String,
    /// New amount (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount: Option<f64>,
    /// New limit/trigger price (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_price: Option<f64>,
    /// New order duration (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_duration: Option<SaxoOrderDuration>,
    /// New trailing stop distance (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trailing_stop_distance_to_market: Option<f64>,
    /// New trailing stop step (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trailing_stop_step: Option<f64>,
}

/// Response from `POST /trade/v2/orders`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SaxoOrderResponse {
    /// The created order ID.
    pub order_id: String,
}

// ============================================================================
// Position Types
// ============================================================================

/// Response from `GET /port/v1/netpositions/me`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SaxoNetPositionsResponse {
    /// List of net positions.
    #[serde(default)]
    pub data: Vec<SaxoNetPosition>,
    /// Total count.
    #[serde(default, rename = "__count")]
    pub count: Option<u32>,
}

/// A single net position.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SaxoNetPosition {
    /// Net position ID.
    pub net_position_id: String,
    /// Base data for the position.
    pub net_position_base: SaxoNetPositionBase,
    /// Display and computed data (P&L, etc.).
    #[serde(default)]
    pub net_position_view: Option<serde_json::Value>,
}

/// Core data of a net position.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SaxoNetPositionBase {
    /// Unique instrument identifier.
    pub uic: u32,
    /// Asset type.
    pub asset_type: String,
    /// Signed amount (positive=long, negative=short).
    pub amount: f64,
    /// Average open price.
    #[serde(default)]
    pub average_open_price: f64,
    /// Current market value.
    #[serde(default)]
    pub market_value: f64,
    /// Unrealized P&L.
    #[serde(default)]
    pub profit_loss_on_trade: f64,
    /// Current price.
    #[serde(default)]
    pub current_price: f64,
}

// ============================================================================
// Quote Types
// ============================================================================

/// Response from `GET /trade/v1/infoprices`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SaxoInfoPriceResponse {
    /// Unique instrument identifier.
    pub uic: u32,
    /// Asset type.
    pub asset_type: String,
    /// Quote snapshot.
    pub quote: SaxoQuote,
}

/// A quote snapshot from the info prices endpoint.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SaxoQuote {
    /// Best bid price.
    #[serde(default)]
    pub bid: f64,
    /// Best ask price.
    #[serde(default)]
    pub ask: f64,
    /// Mid price.
    #[serde(default)]
    pub mid: f64,
    /// Delay in seconds (0 = real-time).
    #[serde(default)]
    pub delay_in_minutes: Option<u32>,
    /// Market status: "Open", "Closed", etc.
    #[serde(default)]
    pub market_state: Option<String>,
}

// ============================================================================
// Chart/Bar Types
// ============================================================================

/// Response from `GET /chart/v3/charts`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SaxoChartResponse {
    /// Chart data points (candles).
    #[serde(default)]
    pub data: Vec<SaxoChartDataPoint>,
}

/// A single chart data point (candle).
///
/// The v3 chart API returns bid/ask OHLC prices. Mid prices are computed
/// as `(bid + ask) / 2` via [`SaxoChartDataPoint::mid_open`] etc.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SaxoChartDataPoint {
    /// Candle open time (ISO 8601).
    pub time: String,
    /// Open bid price.
    #[serde(default)]
    pub open_bid: f64,
    /// Open ask price.
    #[serde(default)]
    pub open_ask: f64,
    /// High bid price.
    #[serde(default)]
    pub high_bid: f64,
    /// High ask price.
    #[serde(default)]
    pub high_ask: f64,
    /// Low bid price.
    #[serde(default)]
    pub low_bid: f64,
    /// Low ask price.
    #[serde(default)]
    pub low_ask: f64,
    /// Close bid price.
    #[serde(default)]
    pub close_bid: f64,
    /// Close ask price.
    #[serde(default)]
    pub close_ask: f64,
    /// Volume.
    #[serde(default)]
    pub volume: f64,
}

impl SaxoChartDataPoint {
    /// Mid open price (average of bid and ask).
    pub fn mid_open(&self) -> f64 {
        (self.open_bid + self.open_ask) / 2.0
    }

    /// Mid high price (average of bid and ask).
    pub fn mid_high(&self) -> f64 {
        (self.high_bid + self.high_ask) / 2.0
    }

    /// Mid low price (average of bid and ask).
    pub fn mid_low(&self) -> f64 {
        (self.low_bid + self.low_ask) / 2.0
    }

    /// Mid close price (average of bid and ask).
    pub fn mid_close(&self) -> f64 {
        (self.close_bid + self.close_ask) / 2.0
    }
}

// ============================================================================
// Trade History Types
// ============================================================================

/// Response from `GET /port/v1/closedpositions`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SaxoClosedPositionsResponse {
    /// List of closed positions (trade history).
    #[serde(default)]
    pub data: Vec<SaxoClosedPosition>,
    /// Total count.
    #[serde(default, rename = "__count")]
    pub count: Option<u32>,
}

/// A single closed position (trade).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SaxoClosedPosition {
    /// Closed position ID.
    pub closed_position_id: String,
    /// Details of the closed position.
    pub closed_position: SaxoClosedPositionDetail,
}

/// Detail of a closed position.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SaxoClosedPositionDetail {
    /// Unique instrument identifier.
    pub uic: u32,
    /// Asset type.
    pub asset_type: String,
    /// Buy or sell direction of the opening trade.
    pub buy_sell: String,
    /// Trade amount.
    pub amount: f64,
    /// Opening price.
    #[serde(default)]
    pub open_price: f64,
    /// Closing price.
    #[serde(default)]
    pub close_price: f64,
    /// Realized profit/loss.
    #[serde(default)]
    pub profit_loss: f64,
    /// Commission/costs.
    #[serde(default)]
    pub costs_total: f64,
    /// Execution time (ISO 8601).
    #[serde(default)]
    pub execution_time_close: Option<String>,
}

// ============================================================================
// Order Precheck Types
// ============================================================================

/// Response from `POST /trade/v2/orders/precheck`.
///
/// Same request body as a real order, but Saxo validates without placing.
/// `PreCheckResult` is `"Ok"` on success or an error string on failure.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SaxoPrecheckResponse {
    /// Result of the precheck: `"Ok"` or an error description.
    pub pre_check_result: String,
    /// Estimated cash required for the order (present on success).
    #[serde(default)]
    pub estimated_cash_required: Option<f64>,
    /// Margin utilisation percentage after this order would be placed.
    #[serde(default)]
    pub margin_utilization_pct: Option<f64>,
    /// Error details (present on failure).
    #[serde(default)]
    pub error_info: Option<SaxoPrecheckError>,
}

/// Error details within a precheck failure response.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SaxoPrecheckError {
    /// Saxo error code (e.g., `"InsufficientMargin"`).
    #[serde(default)]
    pub error_code: Option<String>,
    /// Human-readable error message.
    #[serde(default)]
    pub message: Option<String>,
}

// ============================================================================
// WebSocket Subscription Types — Portfolio (Balance / Orders)
// ============================================================================

/// Request body for creating a portfolio subscription (balances or orders).
///
/// Used for `POST /port/v1/balances/subscriptions` and
/// `POST /port/v1/orders/subscriptions`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct SaxoPortfolioSubscriptionRequest {
    /// Account-scoped arguments.
    pub arguments: SaxoPortfolioSubscriptionArguments,
    /// Context ID linking this subscription to a WebSocket connection.
    pub context_id: String,
    /// Unique reference ID for this subscription.
    pub reference_id: String,
    /// Minimum milliseconds between updates.
    pub refresh_rate: u32,
}

/// Arguments for a portfolio subscription (balance or order stream).
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct SaxoPortfolioSubscriptionArguments {
    /// Saxo account key to subscribe to.
    pub account_key: String,
}

// ============================================================================
// WebSocket Subscription Types — Price
// ============================================================================

/// Request body for creating a price subscription via
/// `POST /trade/v1/infoprices/subscriptions`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct SaxoSubscriptionRequest {
    /// Subscription arguments (instrument + asset type).
    pub arguments: SaxoSubscriptionArguments,
    /// Context ID linking this subscription to a WebSocket connection.
    pub context_id: String,
    /// Unique reference ID for this subscription (used in binary frames).
    pub reference_id: String,
    /// Minimum milliseconds between updates (0 = real-time).
    pub refresh_rate: u32,
}

/// Arguments within a subscription request.
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct SaxoSubscriptionArguments {
    /// Saxo unique instrument identifier.
    pub uic: u32,
    /// Asset type (e.g., "FxSpot").
    pub asset_type: String,
}

/// Response from creating a price subscription.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SaxoSubscriptionResponse {
    /// Context ID echoed back.
    pub context_id: String,
    /// Reference ID echoed back.
    pub reference_id: String,
    /// Initial full snapshot of the subscribed data.
    pub snapshot: serde_json::Value,
    /// Inactivity timeout in seconds (server-suggested heartbeat window).
    #[serde(default)]
    pub inactivity_timeout: Option<u32>,
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- Auth tests ---

    #[test]
    fn deserialize_token_response() {
        let json = r#"{
            "access_token": "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.test",
            "token_type": "Bearer",
            "expires_in": 1200,
            "refresh_token": "d9e1a2b3-c4f5-6789-abcd-ef0123456789",
            "refresh_token_expires_in": 3600
        }"#;
        let resp: SaxoTokenResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.token_type, "Bearer");
        assert_eq!(resp.expires_in, 1200);
        assert!(resp.refresh_token.is_some());
        assert_eq!(resp.refresh_token_expires_in, Some(3600));
    }

    #[test]
    fn deserialize_token_response_without_refresh() {
        let json = r#"{
            "access_token": "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.test",
            "token_type": "Bearer",
            "expires_in": 1200
        }"#;
        let resp: SaxoTokenResponse = serde_json::from_str(json).unwrap();
        assert!(resp.refresh_token.is_none());
        assert!(resp.refresh_token_expires_in.is_none());
    }

    #[test]
    fn deserialize_token_error_response() {
        let json = r#"{
            "error": "invalid_grant",
            "error_description": "The refresh token has expired"
        }"#;
        let resp: SaxoTokenErrorResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.error, "invalid_grant");
        assert_eq!(
            resp.error_description.as_deref(),
            Some("The refresh token has expired")
        );
    }

    // --- REST DTO tests ---

    #[test]
    fn deserialize_balance_response_pascal_case() {
        let json = r#"{
            "TotalValue": 100250.50,
            "CashBalance": 100000.00,
            "MarginUsedByCurrentPositions": 2500.00,
            "MarginAvailable": 97750.50,
            "UnrealizedPositionsValue": 250.50,
            "Currency": "USD"
        }"#;
        let resp: SaxoBalanceResponse = serde_json::from_str(json).unwrap();
        assert!((resp.total_value - 100_250.50).abs() < f64::EPSILON);
        assert!((resp.cash_balance - 100_000.00).abs() < f64::EPSILON);
        assert!((resp.margin_used_by_current_positions - 2500.0).abs() < f64::EPSILON);
        assert_eq!(resp.currency, "USD");
    }

    #[test]
    fn serialize_order_request_pascal_case() {
        let req = SaxoOrderRequest {
            uic: 21,
            asset_type: "FxSpot".to_string(),
            buy_sell: "Buy".to_string(),
            amount: 100_000.0,
            order_type: "Market".to_string(),
            manual_order: false,
            account_key: "acc123".to_string(),
            order_duration: SaxoOrderDuration {
                duration_type: "FillOrKill".to_string(),
            },
            order_price: None,
            stop_limit_price: None,
            trailing_stop_distance_to_market: None,
            trailing_stop_step: None,
            external_reference: Some("my-order-1".to_string()),
            orders: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["Uic"], 21);
        assert_eq!(json["AssetType"], "FxSpot");
        assert_eq!(json["ManualOrder"], false);
        assert_eq!(json["OrderDuration"]["DurationType"], "FillOrKill");
        assert_eq!(json["ExternalReference"], "my-order-1");
        assert!(json.get("OrderPrice").is_none());
    }

    #[test]
    fn serialize_subscription_request_pascal_case() {
        let req = SaxoSubscriptionRequest {
            arguments: SaxoSubscriptionArguments {
                uic: 21,
                asset_type: "FxSpot".to_string(),
            },
            context_id: "ctx-123".to_string(),
            reference_id: "price_eurusd".to_string(),
            refresh_rate: 0,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["Arguments"]["Uic"], 21);
        assert_eq!(json["Arguments"]["AssetType"], "FxSpot");
        assert_eq!(json["ContextId"], "ctx-123");
        assert_eq!(json["ReferenceId"], "price_eurusd");
        assert_eq!(json["RefreshRate"], 0);
    }

    #[test]
    fn deserialize_subscription_response_pascal_case() {
        let json = r#"{
            "ContextId": "ctx-123",
            "ReferenceId": "price_eurusd",
            "Snapshot": {
                "Quote": {"Bid": 1.08500, "Ask": 1.08520, "Mid": 1.08510}
            },
            "InactivityTimeout": 120
        }"#;
        let resp: SaxoSubscriptionResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.context_id, "ctx-123");
        assert_eq!(resp.reference_id, "price_eurusd");
        assert!(resp.snapshot["Quote"]["Bid"].is_number());
        assert_eq!(resp.inactivity_timeout, Some(120));
    }

    #[test]
    fn deserialize_precheck_ok_response() {
        let json = r#"{
            "PreCheckResult": "Ok",
            "EstimatedCashRequired": 2500.50,
            "MarginUtilizationPct": 45.2
        }"#;
        let resp: SaxoPrecheckResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.pre_check_result, "Ok");
        assert!((resp.estimated_cash_required.unwrap() - 2500.50).abs() < f64::EPSILON);
        assert!(resp.error_info.is_none());
    }

    #[test]
    fn deserialize_precheck_error_response() {
        let json = r#"{
            "PreCheckResult": "Error",
            "ErrorInfo": {
                "ErrorCode": "InsufficientMargin",
                "Message": "Insufficient margin for this order"
            }
        }"#;
        let resp: SaxoPrecheckResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.pre_check_result, "Error");
        assert!(resp.estimated_cash_required.is_none());
        let err = resp.error_info.unwrap();
        assert_eq!(err.error_code.as_deref(), Some("InsufficientMargin"));
    }
}
