//! Oanda v20 API request, response, and streaming types.
//!
//! These types map to the Oanda v20 API responses, requests, and streaming messages.
//! They are used internally by the `OandaAdapter` and `OandaWebSocketProvider`.
#![allow(dead_code)] // Broker API response types — all fields required for deserialization

use serde::{Deserialize, Serialize};

// ============================================================================
// Account Types
// ============================================================================

/// Response from `GET /v3/accounts/{id}/summary`.
#[derive(Debug, Deserialize)]
pub struct OandaAccountResponse {
    /// Account details.
    pub account: OandaAccount,
}

/// Oanda account summary.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OandaAccount {
    /// Account balance.
    pub balance: String,
    /// Net asset value (balance + unrealized P&L).
    #[serde(rename = "NAV")]
    pub nav: String,
    /// Margin currently in use.
    pub margin_used: String,
    /// Margin available for new trades.
    pub margin_available: String,
    /// Unrealized profit/loss across all open trades.
    #[serde(rename = "unrealizedPL")]
    pub unrealized_pl: String,
    /// Whether the account uses hedging mode.
    pub hedging_enabled: bool,
    /// Account currency (e.g., "USD").
    pub currency: String,
}

// ============================================================================
// Order Types -- Responses
// ============================================================================

/// Response from `GET /v3/accounts/{id}/orders`.
#[derive(Debug, Deserialize)]
pub struct OandaOrdersResponse {
    /// List of orders.
    pub orders: Vec<OandaOrder>,
}

/// An Oanda order.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OandaOrder {
    /// Order ID.
    pub id: String,
    /// Order type: MARKET, LIMIT, STOP, MARKET_IF_TOUCHED, TRAILING_STOP_LOSS, etc.
    #[serde(rename = "type")]
    pub order_type: String,
    /// Instrument (e.g., "EUR_USD").
    #[serde(default)]
    pub instrument: Option<String>,
    /// Signed units (positive=buy, negative=sell). Present on most order types.
    #[serde(default)]
    pub units: Option<String>,
    /// Trigger price for limit/stop orders.
    #[serde(default)]
    pub price: Option<String>,
    /// Time in force: GTC, GFD, IOC, FOK.
    #[serde(default)]
    pub time_in_force: Option<String>,
    /// Order state: PENDING, FILLED, TRIGGERED, CANCELLED.
    pub state: String,
    /// Creation time (RFC 3339).
    pub create_time: String,
    /// Stop loss on fill configuration.
    #[serde(default)]
    pub stop_loss_on_fill: Option<OandaStopLossOnFill>,
    /// Take profit on fill configuration.
    #[serde(default)]
    pub take_profit_on_fill: Option<OandaTakeProfitOnFill>,
    /// Trailing stop loss on fill configuration.
    #[serde(default)]
    pub trailing_stop_loss_on_fill: Option<OandaTrailingStopLossOnFill>,
    /// Trade ID this order is attached to (for dependent orders like SL/TP).
    #[serde(default)]
    pub trade_id: Option<String>,
    /// Client extensions.
    #[serde(default)]
    pub client_extensions: Option<OandaClientExtensions>,
}

/// Response from `POST /v3/accounts/{id}/orders` (order creation).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OandaCreateOrderResponse {
    /// Transaction for order creation.
    #[serde(default)]
    pub order_create_transaction: Option<OandaTransaction>,
    /// Transaction for immediate fill (market orders).
    #[serde(default)]
    pub order_fill_transaction: Option<OandaTransaction>,
    /// Transaction for order rejection.
    #[serde(default)]
    pub order_reject_transaction: Option<OandaTransaction>,
    /// Transaction for order cancellation (e.g., FOK not filled).
    #[serde(default)]
    pub order_cancel_transaction: Option<OandaTransaction>,
}

/// An Oanda transaction (used in order creation responses and streams).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OandaTransaction {
    /// Transaction ID.
    pub id: String,
    /// Transaction type (e.g., MARKET_ORDER, ORDER_FILL, ORDER_CANCEL).
    #[serde(rename = "type")]
    pub transaction_type: String,
    /// Instrument.
    #[serde(default)]
    pub instrument: Option<String>,
    /// Signed units.
    #[serde(default)]
    pub units: Option<String>,
    /// Price.
    #[serde(default)]
    pub price: Option<String>,
    /// Transaction time (RFC 3339).
    #[serde(default)]
    pub time: Option<String>,
    /// Related order ID.
    #[serde(default)]
    pub order_id: Option<String>,
    /// Trade that was opened or closed.
    #[serde(default)]
    pub trade_id: Option<String>,
    /// Reason for this transaction.
    #[serde(default)]
    pub reason: Option<String>,
    /// Rejection reason (present in reject transactions).
    #[serde(default)]
    pub reject_reason: Option<String>,
}

// ============================================================================
// Position Types
// ============================================================================

/// Response from `GET /v3/accounts/{id}/openPositions`.
#[derive(Debug, Deserialize)]
pub struct OandaPositionsResponse {
    /// List of open positions.
    pub positions: Vec<OandaPosition>,
}

/// Response from `GET /v3/accounts/{id}/positions/{instrument}`.
#[derive(Debug, Deserialize)]
pub struct OandaPositionResponse {
    /// Single position.
    pub position: OandaPosition,
}

/// An Oanda position (aggregated per instrument).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OandaPosition {
    /// Instrument (e.g., "EUR_USD").
    pub instrument: String,
    /// Long side of the position.
    pub long: OandaPositionSide,
    /// Short side of the position.
    pub short: OandaPositionSide,
    /// Unrealized P&L for the entire position.
    #[serde(default, rename = "unrealizedPL")]
    pub unrealized_pl: Option<String>,
}

/// One side (long or short) of an Oanda position.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OandaPositionSide {
    /// Number of units (always positive or "0").
    pub units: String,
    /// Average entry price.
    #[serde(default)]
    pub average_price: Option<String>,
    /// Unrealized P&L for this side.
    #[serde(default, rename = "unrealizedPL")]
    pub unrealized_pl: Option<String>,
}

// ============================================================================
// Pricing Types
// ============================================================================

/// Response from `GET /v3/accounts/{id}/pricing`.
#[derive(Debug, Deserialize)]
pub struct OandaPricingResponse {
    /// List of prices.
    pub prices: Vec<OandaPrice>,
}

/// A price quote from Oanda.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OandaPrice {
    /// Instrument (e.g., "EUR_USD").
    pub instrument: String,
    /// Bid price tiers (best first).
    pub bids: Vec<OandaPriceBucket>,
    /// Ask price tiers (best first).
    pub asks: Vec<OandaPriceBucket>,
    /// Timestamp.
    pub time: String,
}

/// A price tier in the order book.
#[derive(Debug, Deserialize)]
pub struct OandaPriceBucket {
    /// Price at this tier.
    pub price: String,
    /// Liquidity available at this price.
    pub liquidity: i64,
}

// ============================================================================
// Candle Types
// ============================================================================

/// Response from `GET /v3/instruments/{instrument}/candles`.
#[derive(Debug, Deserialize)]
pub struct OandaCandlesResponse {
    /// List of candles.
    pub candles: Vec<OandaCandle>,
}

/// An Oanda candle (OHLC bar).
#[derive(Debug, Deserialize)]
pub struct OandaCandle {
    /// Candle timestamp (RFC 3339).
    pub time: String,
    /// Mid-point OHLC data.
    #[serde(default)]
    pub mid: Option<OandaCandleData>,
    /// Bid OHLC data.
    #[serde(default)]
    pub bid: Option<OandaCandleData>,
    /// Ask OHLC data.
    #[serde(default)]
    pub ask: Option<OandaCandleData>,
    /// Tick volume for the candle.
    pub volume: i64,
    /// Whether the candle is complete.
    pub complete: bool,
}

/// OHLC data within a candle.
#[derive(Debug, Deserialize)]
pub struct OandaCandleData {
    /// Open price.
    pub o: String,
    /// High price.
    pub h: String,
    /// Low price.
    pub l: String,
    /// Close price.
    pub c: String,
}

// ============================================================================
// Trade Types
// ============================================================================

/// Response from `GET /v3/accounts/{id}/trades`.
#[derive(Debug, Deserialize)]
pub struct OandaTradesResponse {
    /// List of trades.
    pub trades: Vec<OandaTrade>,
}

/// An Oanda trade (individual fill/position unit).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OandaTrade {
    /// Trade ID.
    pub id: String,
    /// Instrument.
    pub instrument: String,
    /// Current units (signed; may differ from initial if partially closed).
    pub current_units: String,
    /// Initial units when the trade was opened.
    #[serde(default)]
    pub initial_units: Option<String>,
    /// Entry price.
    pub price: String,
    /// Time the trade was opened (RFC 3339).
    pub open_time: String,
    /// Trade state: OPEN, CLOSED, CLOSE_WHEN_TRADEABLE.
    pub state: String,
    /// Unrealized P&L.
    #[serde(default, rename = "unrealizedPL")]
    pub unrealized_pl: Option<String>,
}

// ============================================================================
// Request Types
// ============================================================================

/// Wrapper for order creation requests.
#[derive(Debug, Serialize)]
pub struct OandaOrderRequestWrapper {
    /// The order request.
    pub order: OandaOrderRequest,
}

/// An Oanda order request body.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OandaOrderRequest {
    /// Order type: MARKET, LIMIT, STOP, MARKET_IF_TOUCHED.
    #[serde(rename = "type")]
    pub order_type: String,
    /// Instrument (e.g., "EUR_USD").
    pub instrument: String,
    /// Signed units (positive=buy, negative=sell).
    pub units: String,
    /// Time in force: GTC, GFD, IOC, FOK.
    pub time_in_force: String,
    /// Trigger price (for limit/stop orders).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<String>,
    /// Stop loss to create on fill.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_loss_on_fill: Option<OandaStopLossOnFill>,
    /// Take profit to create on fill.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub take_profit_on_fill: Option<OandaTakeProfitOnFill>,
    /// Trailing stop loss to create on fill.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trailing_stop_loss_on_fill: Option<OandaTrailingStopLossOnFill>,
    /// Client extensions (optional tag/comment).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_extensions: Option<OandaClientExtensions>,
}

/// Stop loss on fill parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OandaStopLossOnFill {
    /// Stop loss price.
    pub price: String,
    /// Time in force for the stop loss order (default GTC).
    #[serde(default = "default_gtc")]
    pub time_in_force: String,
}

/// Take profit on fill parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OandaTakeProfitOnFill {
    /// Take profit price.
    pub price: String,
    /// Time in force for the take profit order (default GTC).
    #[serde(default = "default_gtc")]
    pub time_in_force: String,
}

/// Trailing stop loss on fill parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OandaTrailingStopLossOnFill {
    /// Distance from current price (in price units).
    pub distance: String,
    /// Time in force (default GTC).
    #[serde(default = "default_gtc")]
    pub time_in_force: String,
}

fn default_gtc() -> String {
    "GTC".to_string()
}

/// Request body for closing a position.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OandaClosePositionRequest {
    /// Long units to close ("ALL" or a specific amount).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub long_units: Option<String>,
    /// Short units to close ("ALL" or a specific amount).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub short_units: Option<String>,
}

/// Request body for closing a trade.
#[derive(Debug, Serialize)]
pub struct OandaCloseTradeRequest {
    /// Units to close (or omit for all).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub units: Option<String>,
}

/// Client extensions for orders/trades.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OandaClientExtensions {
    /// Client-assigned ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Tag for grouping.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// Free-form comment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

// ============================================================================
// Error Response
// ============================================================================

/// Oanda error response body.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OandaErrorResponse {
    /// Error message from the API.
    pub error_message: Option<String>,
}

// ============================================================================
// Streaming Types (HTTP chunked transfer -- NDJSON)
// ============================================================================

/// A message from the Oanda pricing stream (NDJSON line).
///
/// Oanda streams prices as newline-delimited JSON over HTTP chunked transfer.
/// Each line is either a price update or a heartbeat.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum OandaPriceStreamMessage {
    /// A price update with bid/ask tiers.
    #[serde(rename = "PRICE")]
    Price(OandaStreamPrice),
    /// A heartbeat indicating the stream is alive.
    #[serde(rename = "HEARTBEAT")]
    Heartbeat(OandaStreamHeartbeat),
}

/// A streaming price update from the pricing endpoint.
#[derive(Debug, Deserialize)]
pub struct OandaStreamPrice {
    /// Instrument (e.g., "EUR_USD").
    pub instrument: String,
    /// Bid price tiers (best first).
    pub bids: Vec<OandaPriceBucket>,
    /// Ask price tiers (best first).
    pub asks: Vec<OandaPriceBucket>,
    /// Timestamp (RFC 3339).
    pub time: String,
    /// Whether the instrument is currently tradeable.
    #[serde(default)]
    pub tradeable: Option<bool>,
}

/// A raw line from the Oanda transaction stream (NDJSON).
///
/// Oanda streams all messages (transactions AND heartbeats) with a `type` field.
/// Heartbeats have `"type": "HEARTBEAT"`, while transactions have their transaction
/// type directly (e.g., `"type": "ORDER_FILL"`). We deserialize all lines into this
/// unified struct and dispatch on the `transaction_type` field.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OandaTransactionStreamLine {
    /// Message/transaction type (e.g., "ORDER_FILL", "HEARTBEAT").
    #[serde(rename = "type")]
    pub transaction_type: String,
    /// Transaction ID (absent on heartbeats).
    #[serde(default)]
    pub id: Option<String>,
    /// Instrument (present on most transaction types).
    #[serde(default)]
    pub instrument: Option<String>,
    /// Signed units (positive=buy, negative=sell).
    #[serde(default)]
    pub units: Option<String>,
    /// Price (fill price for ORDER_FILL, trigger price for limit/stop).
    #[serde(default)]
    pub price: Option<String>,
    /// Transaction time (RFC 3339).
    #[serde(default)]
    pub time: Option<String>,
    /// Related order ID.
    #[serde(default)]
    pub order_id: Option<String>,
    /// Reason for this transaction.
    #[serde(default)]
    pub reason: Option<String>,
    /// Rejection reason (present in reject transactions).
    #[serde(default)]
    pub reject_reason: Option<String>,
    /// Last transaction ID (present on heartbeats).
    #[serde(default, rename = "lastTransactionID")]
    pub last_transaction_id: Option<String>,
}

/// Heartbeat from the pricing stream.
#[derive(Debug, Deserialize)]
pub struct OandaStreamHeartbeat {
    /// Timestamp (RFC 3339).
    pub time: String,
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_account_response() {
        let json = r#"{
            "account": {
                "balance": "100000.0000",
                "NAV": "100250.5000",
                "marginUsed": "2500.0000",
                "marginAvailable": "97750.5000",
                "unrealizedPL": "250.5000",
                "hedgingEnabled": false,
                "currency": "USD"
            }
        }"#;
        let resp: OandaAccountResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.account.balance, "100000.0000");
        assert_eq!(resp.account.nav, "100250.5000");
        assert_eq!(resp.account.margin_used, "2500.0000");
        assert!(!resp.account.hedging_enabled);
        assert_eq!(resp.account.currency, "USD");
    }

    #[test]
    fn deserialize_order() {
        let json = r#"{
            "id": "6356",
            "type": "LIMIT",
            "instrument": "EUR_USD",
            "units": "10000",
            "price": "1.08500",
            "timeInForce": "GTC",
            "state": "PENDING",
            "createTime": "2024-01-15T10:30:00.000000000Z",
            "stopLossOnFill": {
                "price": "1.08000",
                "timeInForce": "GTC"
            },
            "takeProfitOnFill": {
                "price": "1.09500",
                "timeInForce": "GTC"
            }
        }"#;
        let order: OandaOrder = serde_json::from_str(json).unwrap();
        assert_eq!(order.id, "6356");
        assert_eq!(order.order_type, "LIMIT");
        assert_eq!(order.instrument.as_deref(), Some("EUR_USD"));
        assert_eq!(order.units.as_deref(), Some("10000"));
        assert!(order.stop_loss_on_fill.is_some());
        assert!(order.take_profit_on_fill.is_some());
    }

    #[test]
    fn deserialize_order_without_optional_fields() {
        let json = r#"{
            "id": "100",
            "type": "STOP_LOSS",
            "state": "PENDING",
            "createTime": "2024-01-15T10:30:00.000000000Z",
            "tradeId": "99",
            "price": "1.08000"
        }"#;
        let order: OandaOrder = serde_json::from_str(json).unwrap();
        assert_eq!(order.id, "100");
        assert_eq!(order.order_type, "STOP_LOSS");
        assert!(order.instrument.is_none());
        assert!(order.units.is_none());
        assert_eq!(order.trade_id.as_deref(), Some("99"));
    }

    #[test]
    fn deserialize_create_order_response_market_fill() {
        let json = r#"{
            "orderCreateTransaction": {
                "id": "6357",
                "type": "MARKET_ORDER",
                "instrument": "EUR_USD",
                "units": "10000",
                "time": "2024-01-15T10:30:00.000000000Z"
            },
            "orderFillTransaction": {
                "id": "6358",
                "type": "ORDER_FILL",
                "instrument": "EUR_USD",
                "units": "10000",
                "price": "1.08525",
                "time": "2024-01-15T10:30:00.000000000Z",
                "orderId": "6357",
                "tradeId": "6358"
            }
        }"#;
        let resp: OandaCreateOrderResponse = serde_json::from_str(json).unwrap();
        assert!(resp.order_create_transaction.is_some());
        assert!(resp.order_fill_transaction.is_some());
        assert!(resp.order_reject_transaction.is_none());
        let fill = resp.order_fill_transaction.unwrap();
        assert_eq!(fill.price.as_deref(), Some("1.08525"));
    }

    #[test]
    fn deserialize_create_order_response_rejection() {
        let json = r#"{
            "orderRejectTransaction": {
                "id": "6359",
                "type": "MARKET_ORDER_REJECT",
                "instrument": "EUR_USD",
                "units": "10000",
                "time": "2024-01-15T10:30:00.000000000Z",
                "rejectReason": "INSUFFICIENT_MARGIN"
            }
        }"#;
        let resp: OandaCreateOrderResponse = serde_json::from_str(json).unwrap();
        assert!(resp.order_create_transaction.is_none());
        assert!(resp.order_reject_transaction.is_some());
        let reject = resp.order_reject_transaction.unwrap();
        assert_eq!(reject.reject_reason.as_deref(), Some("INSUFFICIENT_MARGIN"));
    }

    #[test]
    fn deserialize_positions_response() {
        let json = r#"{
            "positions": [{
                "instrument": "EUR_USD",
                "long": {
                    "units": "10000",
                    "averagePrice": "1.08525",
                    "unrealizedPL": "125.50"
                },
                "short": {
                    "units": "0"
                },
                "unrealizedPL": "125.50"
            }]
        }"#;
        let resp: OandaPositionsResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.positions.len(), 1);
        let pos = &resp.positions[0];
        assert_eq!(pos.instrument, "EUR_USD");
        assert_eq!(pos.long.units, "10000");
        assert_eq!(pos.long.average_price.as_deref(), Some("1.08525"));
        assert_eq!(pos.short.units, "0");
    }

    #[test]
    fn deserialize_single_position_response() {
        let json = r#"{
            "position": {
                "instrument": "GBP_USD",
                "long": {
                    "units": "0"
                },
                "short": {
                    "units": "5000",
                    "averagePrice": "1.27200",
                    "unrealizedPL": "-30.00"
                }
            }
        }"#;
        let resp: OandaPositionResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.position.instrument, "GBP_USD");
        assert_eq!(resp.position.short.units, "5000");
    }

    #[test]
    fn deserialize_pricing_response() {
        let json = r#"{
            "prices": [{
                "instrument": "EUR_USD",
                "bids": [{"price": "1.08520", "liquidity": 1000000}],
                "asks": [{"price": "1.08530", "liquidity": 1000000}],
                "time": "2024-01-15T10:30:00.000000000Z"
            }]
        }"#;
        let resp: OandaPricingResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.prices.len(), 1);
        assert_eq!(resp.prices[0].bids[0].price, "1.08520");
        assert_eq!(resp.prices[0].asks[0].price, "1.08530");
        assert_eq!(resp.prices[0].bids[0].liquidity, 1_000_000);
    }

    #[test]
    fn deserialize_candles_response() {
        let json = r#"{
            "candles": [{
                "time": "2024-01-15T10:00:00.000000000Z",
                "mid": {"o": "1.08500", "h": "1.08600", "l": "1.08400", "c": "1.08550"},
                "volume": 1234,
                "complete": true
            }]
        }"#;
        let resp: OandaCandlesResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.candles.len(), 1);
        let candle = &resp.candles[0];
        assert!(candle.complete);
        assert_eq!(candle.volume, 1234);
        let mid = candle.mid.as_ref().unwrap();
        assert_eq!(mid.o, "1.08500");
        assert_eq!(mid.c, "1.08550");
    }

    #[test]
    fn deserialize_trades_response() {
        let json = r#"{
            "trades": [{
                "id": "6358",
                "instrument": "EUR_USD",
                "currentUnits": "10000",
                "initialUnits": "10000",
                "price": "1.08525",
                "openTime": "2024-01-15T10:30:00.000000000Z",
                "state": "OPEN",
                "unrealizedPL": "50.00"
            }]
        }"#;
        let resp: OandaTradesResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.trades.len(), 1);
        assert_eq!(resp.trades[0].id, "6358");
        assert_eq!(resp.trades[0].current_units, "10000");
        assert_eq!(resp.trades[0].state, "OPEN");
    }

    #[test]
    fn serialize_order_request_market() {
        let req = OandaOrderRequestWrapper {
            order: OandaOrderRequest {
                order_type: "MARKET".to_string(),
                instrument: "EUR_USD".to_string(),
                units: "10000".to_string(),
                time_in_force: "FOK".to_string(),
                price: None,
                stop_loss_on_fill: Some(OandaStopLossOnFill {
                    price: "1.08000".to_string(),
                    time_in_force: "GTC".to_string(),
                }),
                take_profit_on_fill: Some(OandaTakeProfitOnFill {
                    price: "1.09500".to_string(),
                    time_in_force: "GTC".to_string(),
                }),
                trailing_stop_loss_on_fill: None,
                client_extensions: None,
            },
        };
        let json = serde_json::to_value(&req).unwrap();
        let order = &json["order"];
        assert_eq!(order["type"], "MARKET");
        assert_eq!(order["instrument"], "EUR_USD");
        assert_eq!(order["units"], "10000");
        assert_eq!(order["timeInForce"], "FOK");
        assert_eq!(order["stopLossOnFill"]["price"], "1.08000");
        assert_eq!(order["takeProfitOnFill"]["price"], "1.09500");
        // Trailing stop should be absent
        assert!(order.get("trailingStopLossOnFill").is_none());
    }

    #[test]
    fn serialize_order_request_limit_no_bracket() {
        let req = OandaOrderRequestWrapper {
            order: OandaOrderRequest {
                order_type: "LIMIT".to_string(),
                instrument: "GBP_USD".to_string(),
                units: "-5000".to_string(),
                time_in_force: "GTC".to_string(),
                price: Some("1.27500".to_string()),
                stop_loss_on_fill: None,
                take_profit_on_fill: None,
                trailing_stop_loss_on_fill: None,
                client_extensions: None,
            },
        };
        let json = serde_json::to_value(&req).unwrap();
        let order = &json["order"];
        assert_eq!(order["type"], "LIMIT");
        assert_eq!(order["units"], "-5000");
        assert_eq!(order["price"], "1.27500");
        // No bracket fields
        assert!(order.get("stopLossOnFill").is_none());
        assert!(order.get("takeProfitOnFill").is_none());
    }

    #[test]
    fn serialize_close_position_request() {
        let req = OandaClosePositionRequest {
            long_units: Some("ALL".to_string()),
            short_units: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["longUnits"], "ALL");
        assert!(json.get("shortUnits").is_none());
    }

    #[test]
    fn serialize_client_extensions() {
        let ext = OandaClientExtensions {
            id: Some("my-order-1".to_string()),
            tag: Some("strategy-a".to_string()),
            comment: None,
        };
        let json = serde_json::to_value(&ext).unwrap();
        assert_eq!(json["id"], "my-order-1");
        assert_eq!(json["tag"], "strategy-a");
        assert!(json.get("comment").is_none());
    }

    #[test]
    fn deserialize_error_response() {
        let json = r#"{"errorMessage": "Invalid value specified for 'units'"}"#;
        let resp: OandaErrorResponse = serde_json::from_str(json).unwrap();
        assert_eq!(
            resp.error_message.as_deref(),
            Some("Invalid value specified for 'units'")
        );
    }

    #[test]
    fn deserialize_transaction() {
        let json = r#"{
            "id": "6360",
            "type": "ORDER_FILL",
            "instrument": "EUR_USD",
            "units": "10000",
            "price": "1.08525",
            "time": "2024-01-15T10:30:00.000000000Z",
            "orderId": "6357",
            "tradeId": "6358"
        }"#;
        let tx: OandaTransaction = serde_json::from_str(json).unwrap();
        assert_eq!(tx.id, "6360");
        assert_eq!(tx.transaction_type, "ORDER_FILL");
        assert_eq!(tx.order_id.as_deref(), Some("6357"));
        assert_eq!(tx.trade_id.as_deref(), Some("6358"));
    }

    // ========================================================================
    // Streaming type tests
    // ========================================================================

    #[test]
    fn deserialize_price_stream_message() {
        let json = r#"{
            "type": "PRICE",
            "instrument": "EUR_USD",
            "bids": [{"price": "1.08520", "liquidity": 1000000}],
            "asks": [{"price": "1.08530", "liquidity": 1000000}],
            "time": "2024-01-15T10:30:00.000000000Z",
            "tradeable": true
        }"#;
        let msg: OandaPriceStreamMessage = serde_json::from_str(json).unwrap();
        match msg {
            OandaPriceStreamMessage::Price(p) => {
                assert_eq!(p.instrument, "EUR_USD");
                assert_eq!(p.bids.len(), 1);
                assert_eq!(p.bids[0].price, "1.08520");
                assert_eq!(p.asks[0].price, "1.08530");
                assert_eq!(p.tradeable, Some(true));
            }
            OandaPriceStreamMessage::Heartbeat(_) => panic!("Expected Price"),
        }
    }

    #[test]
    fn deserialize_price_stream_heartbeat() {
        let json = r#"{
            "type": "HEARTBEAT",
            "time": "2024-01-15T10:30:05.000000000Z"
        }"#;
        let msg: OandaPriceStreamMessage = serde_json::from_str(json).unwrap();
        match msg {
            OandaPriceStreamMessage::Heartbeat(h) => {
                assert_eq!(h.time, "2024-01-15T10:30:05.000000000Z");
            }
            OandaPriceStreamMessage::Price(_) => panic!("Expected Heartbeat"),
        }
    }

    #[test]
    fn deserialize_transaction_stream_fill() {
        let tx: OandaTransactionStreamLine = serde_json::from_str(
            r#"{
            "id": "6360",
            "type": "ORDER_FILL",
            "instrument": "EUR_USD",
            "units": "10000",
            "price": "1.08525",
            "time": "2024-01-15T10:30:00.000000000Z",
            "orderId": "6357",
            "reason": "MARKET_ORDER"
        }"#,
        )
        .unwrap();
        assert_eq!(tx.id.as_deref(), Some("6360"));
        assert_eq!(tx.transaction_type, "ORDER_FILL");
        assert_eq!(tx.instrument.as_deref(), Some("EUR_USD"));
        assert_eq!(tx.units.as_deref(), Some("10000"));
        assert_eq!(tx.price.as_deref(), Some("1.08525"));
        assert_eq!(tx.reason.as_deref(), Some("MARKET_ORDER"));
    }

    #[test]
    fn deserialize_transaction_stream_cancel() {
        let json = r#"{
            "id": "6365",
            "type": "ORDER_CANCEL",
            "instrument": "EUR_USD",
            "orderId": "6356",
            "time": "2024-01-15T11:00:00.000000000Z",
            "reason": "CLIENT_REQUEST"
        }"#;
        let tx: OandaTransactionStreamLine = serde_json::from_str(json).unwrap();
        assert_eq!(tx.transaction_type, "ORDER_CANCEL");
        assert_eq!(tx.order_id.as_deref(), Some("6356"));
        assert!(tx.units.is_none());
        assert!(tx.price.is_none());
    }

    #[test]
    fn deserialize_transaction_stream_reject() {
        let json = r#"{
            "id": "6370",
            "type": "MARKET_ORDER_REJECT",
            "instrument": "EUR_USD",
            "units": "10000",
            "time": "2024-01-15T11:30:00.000000000Z",
            "rejectReason": "INSUFFICIENT_MARGIN"
        }"#;
        let tx: OandaTransactionStreamLine = serde_json::from_str(json).unwrap();
        assert_eq!(tx.transaction_type, "MARKET_ORDER_REJECT");
        assert_eq!(tx.reject_reason.as_deref(), Some("INSUFFICIENT_MARGIN"));
    }

    #[test]
    fn deserialize_transaction_stream_heartbeat() {
        let json = r#"{
            "type": "HEARTBEAT",
            "time": "2024-01-15T10:30:05.000000000Z",
            "lastTransactionID": "6360"
        }"#;
        let line: OandaTransactionStreamLine = serde_json::from_str(json).unwrap();
        assert_eq!(line.transaction_type, "HEARTBEAT");
        assert_eq!(line.time.as_deref(), Some("2024-01-15T10:30:05.000000000Z"));
        assert_eq!(line.last_transaction_id.as_deref(), Some("6360"));
    }

    #[test]
    fn deserialize_price_stream_multiple_tiers() {
        let json = r#"{
            "type": "PRICE",
            "instrument": "GBP_USD",
            "bids": [
                {"price": "1.27200", "liquidity": 1000000},
                {"price": "1.27195", "liquidity": 5000000}
            ],
            "asks": [
                {"price": "1.27210", "liquidity": 1000000},
                {"price": "1.27215", "liquidity": 5000000}
            ],
            "time": "2024-01-15T10:30:00.000000000Z"
        }"#;
        let msg: OandaPriceStreamMessage = serde_json::from_str(json).unwrap();
        match msg {
            OandaPriceStreamMessage::Price(p) => {
                assert_eq!(p.bids.len(), 2);
                assert_eq!(p.asks.len(), 2);
                assert_eq!(p.bids[0].price, "1.27200");
                assert_eq!(p.bids[1].liquidity, 5_000_000);
                assert!(p.tradeable.is_none());
            }
            OandaPriceStreamMessage::Heartbeat(_) => panic!("Expected Price"),
        }
    }

    #[test]
    fn deserialize_stream_transaction_minimal() {
        let json = r#"{
            "id": "6380",
            "type": "DAILY_FINANCING"
        }"#;
        let tx: OandaTransactionStreamLine = serde_json::from_str(json).unwrap();
        assert_eq!(tx.id.as_deref(), Some("6380"));
        assert_eq!(tx.transaction_type, "DAILY_FINANCING");
        assert!(tx.instrument.is_none());
        assert!(tx.units.is_none());
        assert!(tx.price.is_none());
        assert!(tx.time.is_none());
        assert!(tx.order_id.is_none());
    }
}
