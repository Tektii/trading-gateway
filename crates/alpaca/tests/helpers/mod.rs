//! Shared test helpers for Alpaca adapter integration tests.
#![allow(dead_code)]

use serde_json::{Value, json};
use tektii_gateway_alpaca::{AlpacaAdapter, AlpacaCredentials};
use tektii_gateway_core::http::RetryConfig;
use tektii_gateway_core::models::TradingPlatform;
use tektii_gateway_core::websocket::messages::WsMessage;
use tektii_gateway_test_support::wiremock_helpers::merge_json;
use tokio::sync::broadcast;

/// Create an `AlpacaAdapter` pointed at a wiremock server.
///
/// Both the trading and data URLs are aimed at the same mock so a single
/// wiremock instance can serve all of an adapter's outbound calls.
pub fn test_adapter(base_url: &str) -> AlpacaAdapter {
    let credentials = AlpacaCredentials::new("test-key", "test-secret")
        .with_base_url(base_url)
        .with_data_url(base_url);
    let (broadcaster, _rx) = broadcast::channel::<WsMessage>(16);
    AlpacaAdapter::new(&credentials, broadcaster, TradingPlatform::AlpacaPaper)
        .expect("Failed to build HTTP client")
        .with_retry_config(RetryConfig::for_tests())
}

// =========================================================================
// Alpaca JSON response builders
// =========================================================================

/// Alpaca account response.
pub fn alpaca_account_json() -> Value {
    json!({
        "id": "account-123",
        "currency": "USD",
        "cash": "100000.00",
        "portfolio_value": "105000.00",
        "buying_power": "200000.00"
    })
}

/// Alpaca order response with optional overrides.
///
/// Provide a JSON object to override any default fields:
/// ```ignore
/// alpaca_order_json(&json!({"status": "filled", "filled_qty": "10"}))
/// ```
pub fn alpaca_order_json(overrides: &Value) -> Value {
    let mut base = json!({
        "id": "order-abc-123",
        "client_order_id": "client-001",
        "symbol": "AAPL",
        "qty": "10",
        "side": "buy",
        "type": "market",
        "time_in_force": "day",
        "status": "new",
        "filled_qty": "0",
        "filled_avg_price": null,
        "limit_price": null,
        "stop_price": null,
        "created_at": "2024-01-15T10:30:00Z",
        "updated_at": "2024-01-15T10:30:00Z",
        "legs": null
    });
    merge_json(&mut base, overrides);
    base
}

/// Alpaca position response.
pub fn alpaca_position_json(symbol: &str) -> Value {
    json!({
        "symbol": symbol,
        "qty": "10",
        "avg_entry_price": "150.00",
        "current_price": "155.00",
        "market_value": "1550.00",
        "unrealized_pl": "50.00",
        "unrealized_plpc": "0.0333"
    })
}

/// Alpaca trade activity (FILL) response.
pub fn alpaca_activity_json(symbol: &str) -> Value {
    json!({
        "id": "activity-001",
        "activity_type": "FILL",
        "order_id": "order-abc-123",
        "symbol": symbol,
        "side": "buy",
        "qty": "10",
        "price": "150.50",
        "transaction_time": "2024-01-15T10:31:00Z"
    })
}

/// Stock quote response.
pub fn alpaca_quote_json() -> Value {
    json!({
        "quote": {
            "t": "2024-01-15T10:30:00Z",
            "bp": 150.0,
            "ap": 150.5,
            "bs": 100.0,
            "as": 200.0
        }
    })
}

/// Crypto quote response.
pub fn alpaca_crypto_quote_json(symbol: &str) -> Value {
    json!({
        "quotes": {
            symbol: {
                "t": "2024-01-15T10:30:00Z",
                "bp": 50000.0,
                "ap": 50100.0,
                "bs": 1.5,
                "as": 2.0
            }
        }
    })
}

/// Stock bars response.
pub fn alpaca_bars_json() -> Value {
    json!({
        "bars": [
            {
                "t": "2024-01-15T10:00:00Z",
                "o": 150.0,
                "h": 152.0,
                "l": 149.5,
                "c": 151.0,
                "v": 10000.0
            }
        ]
    })
}

/// Crypto bars response.
pub fn alpaca_crypto_bars_json(symbol: &str) -> Value {
    json!({
        "bars": {
            symbol: [
                {
                    "t": "2024-01-15T10:00:00Z",
                    "o": 50000.0,
                    "h": 50500.0,
                    "l": 49800.0,
                    "c": 50200.0,
                    "v": 150.0
                }
            ]
        }
    })
}
